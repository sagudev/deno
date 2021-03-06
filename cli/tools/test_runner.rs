// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.

use crate::colors;
use crate::create_main_worker;
use crate::file_fetcher::File;
use crate::flags::Flags;
use crate::fs_util;
use crate::media_type::MediaType;
use crate::module_graph;
use crate::program_state::ProgramState;
use crate::tokio_util;
use crate::tools::coverage::CoverageCollector;
use crate::tools::installer::is_remote_url;
use deno_core::error::AnyError;
use deno_core::futures::future;
use deno_core::futures::stream;
use deno_core::futures::StreamExt;
use deno_core::serde_json::json;
use deno_core::url::Url;
use deno_core::ModuleSpecifier;
use deno_runtime::permissions::Permissions;
use serde::Deserialize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TestResult {
  Ok,
  Ignored,
  Failed(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "camelCase")]
pub enum TestMessage {
  Plan {
    pending: usize,
    filtered: usize,
    only: bool,
  },
  Wait {
    name: String,
  },
  Result {
    name: String,
    duration: usize,
    result: TestResult,
  },
}

trait TestReporter {
  fn visit_message(&mut self, message: TestMessage);
  fn done(&mut self);
}

struct PrettyTestReporter {
  time: Instant,
  failed: usize,
  filtered_out: usize,
  ignored: usize,
  passed: usize,
  measured: usize,
  pending: usize,
  failures: Vec<(String, String)>,
  concurrent: bool,
}

impl PrettyTestReporter {
  fn new(concurrent: bool) -> PrettyTestReporter {
    PrettyTestReporter {
      time: Instant::now(),
      failed: 0,
      filtered_out: 0,
      ignored: 0,
      passed: 0,
      measured: 0,
      pending: 0,
      failures: Vec::new(),
      concurrent,
    }
  }
}

impl TestReporter for PrettyTestReporter {
  fn visit_message(&mut self, message: TestMessage) {
    match &message {
      TestMessage::Plan {
        pending,
        filtered,
        only: _,
      } => {
        println!("running {} tests", pending);
        self.pending += pending;
        self.filtered_out += filtered;
      }

      TestMessage::Wait { name } => {
        if !self.concurrent {
          print!("test {} ...", name);
        }
      }

      TestMessage::Result {
        name,
        duration,
        result,
      } => {
        self.pending -= 1;

        if self.concurrent {
          print!("test {} ...", name);
        }

        match result {
          TestResult::Ok => {
            println!(
              " {} {}",
              colors::green("ok"),
              colors::gray(format!("({}ms)", duration))
            );

            self.passed += 1;
          }
          TestResult::Ignored => {
            println!(
              " {} {}",
              colors::yellow("ignored"),
              colors::gray(format!("({}ms)", duration))
            );

            self.ignored += 1;
          }
          TestResult::Failed(error) => {
            println!(
              " {} {}",
              colors::red("FAILED"),
              colors::gray(format!("({}ms)", duration))
            );

            self.failed += 1;
            self.failures.push((name.to_string(), error.to_string()));
          }
        }
      }
    }
  }

  fn done(&mut self) {
    if !self.failures.is_empty() {
      println!("\nfailures:\n");
      for (name, error) in &self.failures {
        println!("{}", name);
        println!("{}", error);
        println!();
      }

      println!("failures:\n");
      for (name, _) in &self.failures {
        println!("\t{}", name);
      }
    }

    let status = if self.pending > 0 || !self.failures.is_empty() {
      colors::red("FAILED").to_string()
    } else {
      colors::green("ok").to_string()
    };

    println!(
        "\ntest result: {}. {} passed; {} failed; {} ignored; {} measured; {} filtered out {}\n",
        status,
        self.passed,
        self.failed,
        self.ignored,
        self.measured,
        self.filtered_out,
        colors::gray(format!("({}ms)", self.time.elapsed().as_millis())),
      );
  }
}

fn create_reporter(concurrent: bool) -> Box<dyn TestReporter + Send> {
  Box::new(PrettyTestReporter::new(concurrent))
}

fn is_supported(p: &Path) -> bool {
  use std::path::Component;
  if let Some(Component::Normal(basename_os_str)) = p.components().next_back() {
    let basename = basename_os_str.to_string_lossy();
    basename.ends_with("_test.ts")
      || basename.ends_with("_test.tsx")
      || basename.ends_with("_test.js")
      || basename.ends_with("_test.mjs")
      || basename.ends_with("_test.jsx")
      || basename.ends_with(".test.ts")
      || basename.ends_with(".test.tsx")
      || basename.ends_with(".test.js")
      || basename.ends_with(".test.mjs")
      || basename.ends_with(".test.jsx")
      || basename == "test.ts"
      || basename == "test.tsx"
      || basename == "test.js"
      || basename == "test.mjs"
      || basename == "test.jsx"
  } else {
    false
  }
}

pub fn collect_test_module_specifiers(
  include: Vec<String>,
  root_path: &Path,
) -> Result<Vec<Url>, AnyError> {
  let (include_paths, include_urls): (Vec<String>, Vec<String>) =
    include.into_iter().partition(|n| !is_remote_url(n));

  let mut prepared = vec![];

  for path in include_paths {
    let p = fs_util::normalize_path(&root_path.join(path));
    if p.is_dir() {
      let test_files = fs_util::collect_files(&[p], &[], is_supported).unwrap();
      let test_files_as_urls = test_files
        .iter()
        .map(|f| Url::from_file_path(f).unwrap())
        .collect::<Vec<Url>>();
      prepared.extend(test_files_as_urls);
    } else {
      let url = Url::from_file_path(p).unwrap();
      prepared.push(url);
    }
  }

  for remote_url in include_urls {
    let url = Url::parse(&remote_url)?;
    prepared.push(url);
  }

  Ok(prepared)
}

pub async fn run_test_file(
  program_state: Arc<ProgramState>,
  main_module: ModuleSpecifier,
  test_module: ModuleSpecifier,
  permissions: Permissions,
  channel: Sender<TestMessage>,
) -> Result<(), AnyError> {
  let mut worker =
    create_main_worker(&program_state, main_module.clone(), permissions, true);

  {
    let js_runtime = &mut worker.js_runtime;
    js_runtime
      .op_state()
      .borrow_mut()
      .put::<Sender<TestMessage>>(channel.clone());
  }

  let mut maybe_coverage_collector = if let Some(ref coverage_dir) =
    program_state.coverage_dir
  {
    let session = worker.create_inspector_session();
    let coverage_dir = PathBuf::from(coverage_dir);
    let mut coverage_collector = CoverageCollector::new(coverage_dir, session);
    coverage_collector.start_collecting().await?;

    Some(coverage_collector)
  } else {
    None
  };

  let execute_result = worker.execute_module(&main_module).await;
  execute_result?;

  worker.execute("window.dispatchEvent(new Event('load'))")?;

  let execute_result = worker.execute_module(&test_module).await;
  execute_result?;

  worker.run_event_loop().await?;
  worker.execute("window.dispatchEvent(new Event('unload'))")?;

  if let Some(coverage_collector) = maybe_coverage_collector.as_mut() {
    coverage_collector.stop_collecting().await?;
  }

  Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_tests(
  flags: Flags,
  include: Option<Vec<String>>,
  no_run: bool,
  fail_fast: bool,
  quiet: bool,
  allow_none: bool,
  filter: Option<String>,
  concurrent_jobs: usize,
) -> Result<(), AnyError> {
  let program_state = ProgramState::build(flags.clone()).await?;
  let permissions = Permissions::from_options(&flags.clone().into());
  let cwd = std::env::current_dir().expect("No current directory");
  let include = include.unwrap_or_else(|| vec![".".to_string()]);
  let test_modules = collect_test_module_specifiers(include, &cwd)?;

  if test_modules.is_empty() {
    println!("No matching test modules found");
    if !allow_none {
      std::process::exit(1);
    }
    return Ok(());
  }

  let lib = if flags.unstable {
    module_graph::TypeLib::UnstableDenoWindow
  } else {
    module_graph::TypeLib::DenoWindow
  };

  program_state
    .prepare_module_graph(
      test_modules.clone(),
      lib.clone(),
      permissions.clone(),
      program_state.maybe_import_map.clone(),
    )
    .await?;

  if no_run {
    return Ok(());
  }

  // Because scripts, and therefore worker.execute cannot detect unresolved promises at the moment
  // we generate a module for the actual test execution.
  let test_options = json!({
    "disableLog": quiet,
    "filter": filter,
  });

  let test_module = deno_core::resolve_path("$deno$test.js")?;
  let test_source =
    format!("await Deno[Deno.internal].runTests({});", test_options);
  let test_file = File {
    local: test_module.to_file_path().unwrap(),
    maybe_types: None,
    media_type: MediaType::JavaScript,
    source: test_source.clone(),
    specifier: test_module.clone(),
  };

  program_state.file_fetcher.insert_cached(test_file);

  let (sender, receiver) = channel::<TestMessage>();

  let join_handles = test_modules.iter().map(move |main_module| {
    let program_state = program_state.clone();
    let main_module = main_module.clone();
    let test_module = test_module.clone();
    let permissions = permissions.clone();
    let sender = sender.clone();

    tokio::task::spawn_blocking(move || {
      let join_handle = std::thread::spawn(move || {
        let future = run_test_file(
          program_state,
          main_module,
          test_module,
          permissions,
          sender,
        );

        tokio_util::run_basic(future)
      });

      join_handle.join().unwrap()
    })
  });

  let join_futures = stream::iter(join_handles)
    .buffer_unordered(concurrent_jobs)
    .collect::<Vec<Result<Result<(), AnyError>, tokio::task::JoinError>>>();

  let mut reporter = create_reporter(concurrent_jobs > 1);
  let handler = {
    tokio::task::spawn_blocking(move || {
      let mut used_only = false;
      let mut has_error = false;
      let mut planned = 0;
      let mut reported = 0;

      for message in receiver.iter() {
        match message.clone() {
          TestMessage::Plan {
            pending,
            filtered: _,
            only,
          } => {
            if only {
              used_only = true;
            }

            planned += pending;
          }
          TestMessage::Result {
            name: _,
            duration: _,
            result,
          } => {
            reported += 1;

            if let TestResult::Failed(_) = result {
              has_error = true;
            }
          }
          _ => {}
        }

        reporter.visit_message(message);

        if has_error && fail_fast {
          break;
        }
      }

      if planned > reported {
        has_error = true;
      }

      reporter.done();

      if planned > reported {
        has_error = true;
      }

      if used_only {
        println!(
          "{} because the \"only\" option was used\n",
          colors::red("FAILED")
        );

        has_error = true;
      }

      has_error
    })
  };

  let (result, join_results) = future::join(handler, join_futures).await;

  let mut join_errors = join_results.into_iter().filter_map(|join_result| {
    join_result
      .ok()
      .map(|handle_result| handle_result.err())
      .flatten()
  });

  if let Some(e) = join_errors.next() {
    Err(e)
  } else {
    if result.unwrap_or(false) {
      std::process::exit(1);
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_collect_test_module_specifiers() {
    let test_data_path = test_util::root_path().join("cli/tests/subdir");
    let mut matched_urls = collect_test_module_specifiers(
      vec![
        "https://example.com/colors_test.ts".to_string(),
        "./mod1.ts".to_string(),
        "./mod3.js".to_string(),
        "subdir2/mod2.ts".to_string(),
        "http://example.com/printf_test.ts".to_string(),
      ],
      &test_data_path,
    )
    .unwrap();
    let test_data_url =
      Url::from_file_path(test_data_path).unwrap().to_string();

    let expected: Vec<Url> = vec![
      format!("{}/mod1.ts", test_data_url),
      format!("{}/mod3.js", test_data_url),
      format!("{}/subdir2/mod2.ts", test_data_url),
      "http://example.com/printf_test.ts".to_string(),
      "https://example.com/colors_test.ts".to_string(),
    ]
    .into_iter()
    .map(|f| Url::parse(&f).unwrap())
    .collect();
    matched_urls.sort();
    assert_eq!(matched_urls, expected);
  }

  #[test]
  fn test_is_supported() {
    assert!(is_supported(Path::new("tests/subdir/foo_test.ts")));
    assert!(is_supported(Path::new("tests/subdir/foo_test.tsx")));
    assert!(is_supported(Path::new("tests/subdir/foo_test.js")));
    assert!(is_supported(Path::new("tests/subdir/foo_test.jsx")));
    assert!(is_supported(Path::new("bar/foo.test.ts")));
    assert!(is_supported(Path::new("bar/foo.test.tsx")));
    assert!(is_supported(Path::new("bar/foo.test.js")));
    assert!(is_supported(Path::new("bar/foo.test.jsx")));
    assert!(is_supported(Path::new("foo/bar/test.js")));
    assert!(is_supported(Path::new("foo/bar/test.jsx")));
    assert!(is_supported(Path::new("foo/bar/test.ts")));
    assert!(is_supported(Path::new("foo/bar/test.tsx")));
    assert!(!is_supported(Path::new("README.md")));
    assert!(!is_supported(Path::new("lib/typescript.d.ts")));
    assert!(!is_supported(Path::new("notatest.js")));
    assert!(!is_supported(Path::new("NotAtest.ts")));
  }

  #[test]
  fn supports_dirs() {
    // TODO(caspervonb) generate some fixtures in a temporary directory instead, there's no need
    // for this to rely on external fixtures.
    let root = test_util::root_path()
      .join("test_util")
      .join("std")
      .join("http");
    println!("root {:?}", root);
    let mut matched_urls =
      collect_test_module_specifiers(vec![".".to_string()], &root).unwrap();
    matched_urls.sort();
    let root_url = Url::from_file_path(root).unwrap().to_string();
    println!("root_url {}", root_url);
    let expected: Vec<Url> = vec![
      format!("{}/_io_test.ts", root_url),
      format!("{}/cookie_test.ts", root_url),
      format!("{}/file_server_test.ts", root_url),
      format!("{}/racing_server_test.ts", root_url),
      format!("{}/server_test.ts", root_url),
      format!("{}/test.ts", root_url),
    ]
    .into_iter()
    .map(|f| Url::parse(&f).unwrap())
    .collect();
    assert_eq!(matched_urls, expected);
  }
}
