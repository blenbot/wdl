//! Implementation of evaluation for V1 tasks.

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::mem;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use indexmap::IndexMap;
use petgraph::algo::toposort;
use tokio_util::sync::CancellationToken;
use tracing::Level;
use tracing::debug;
use tracing::enabled;
use tracing::info;
use tracing::warn;
use wdl_analysis::diagnostics::multiple_type_mismatch;
use wdl_analysis::diagnostics::unknown_name;
use wdl_analysis::document::Document;
use wdl_analysis::document::TASK_VAR_NAME;
use wdl_analysis::document::Task;
use wdl_analysis::eval::v1::TaskGraphBuilder;
use wdl_analysis::eval::v1::TaskGraphNode;
use wdl_analysis::types::Optional;
use wdl_analysis::types::PrimitiveType;
use wdl_analysis::types::Type;
use wdl_analysis::types::v1::task_hint_types;
use wdl_analysis::types::v1::task_requirement_types;
use wdl_ast::Ast;
use wdl_ast::AstNode;
use wdl_ast::AstToken;
use wdl_ast::Diagnostic;
use wdl_ast::Span;
use wdl_ast::SupportedVersion;
use wdl_ast::v1::CommandPart;
use wdl_ast::v1::CommandSection;
use wdl_ast::v1::Decl;
use wdl_ast::v1::RequirementsSection;
use wdl_ast::v1::RuntimeSection;
use wdl_ast::v1::StrippedCommandPart;
use wdl_ast::v1::TASK_HINT_MAX_CPU;
use wdl_ast::v1::TASK_HINT_MAX_CPU_ALIAS;
use wdl_ast::v1::TASK_HINT_MAX_MEMORY;
use wdl_ast::v1::TASK_HINT_MAX_MEMORY_ALIAS;
use wdl_ast::v1::TASK_REQUIREMENT_CONTAINER;
use wdl_ast::v1::TASK_REQUIREMENT_CONTAINER_ALIAS;
use wdl_ast::v1::TASK_REQUIREMENT_CPU;
use wdl_ast::v1::TASK_REQUIREMENT_MAX_RETRIES;
use wdl_ast::v1::TASK_REQUIREMENT_MAX_RETRIES_ALIAS;
use wdl_ast::v1::TASK_REQUIREMENT_MEMORY;
use wdl_ast::v1::TaskDefinition;
use wdl_ast::v1::TaskHintsSection;
use wdl_ast::version::V1;

use super::ProgressKind;
use crate::Coercible;
use crate::EvaluationContext;
use crate::EvaluationError;
use crate::EvaluationResult;
use crate::Input;
use crate::Outputs;
use crate::PrimitiveValue;
use crate::Scope;
use crate::ScopeIndex;
use crate::ScopeRef;
use crate::TaskExecutionBackend;
use crate::TaskExecutionRoot;
use crate::TaskInputs;
use crate::TaskSpawnRequest;
use crate::TaskValue;
use crate::Value;
use crate::config::Config;
use crate::config::MAX_RETRIES;
use crate::convert_unit_string;
use crate::diagnostics::output_evaluation_failed;
use crate::diagnostics::runtime_type_mismatch;
use crate::diagnostics::task_execution_failed;
use crate::diagnostics::task_localization_failed;
use crate::eval::EvaluatedTask;
use crate::http::Downloader;
use crate::http::HttpDownloader;
use crate::path;
use crate::path::EvaluationPath;
use crate::tree::SyntaxNode;
use crate::v1::ExprEvaluator;

/// The default container requirement.
pub const DEFAULT_TASK_REQUIREMENT_CONTAINER: &str = "ubuntu:latest";
/// The default value for the `cpu` requirement.
pub const DEFAULT_TASK_REQUIREMENT_CPU: f64 = 1.0;
/// The default value for the `memory` requirement.
pub const DEFAULT_TASK_REQUIREMENT_MEMORY: i64 = 2 * 1024 * 1024 * 1024;
/// The default value for the `max_retries` requirement.
pub const DEFAULT_TASK_REQUIREMENT_MAX_RETRIES: u64 = 0;

/// The index of a task's root scope.
const ROOT_SCOPE_INDEX: ScopeIndex = ScopeIndex::new(0);
/// The index of a task's output scope.
const OUTPUT_SCOPE_INDEX: ScopeIndex = ScopeIndex::new(1);
/// The index of the evaluation scope where the WDL 1.2 `task` variable is
/// visible.
const TASK_SCOPE_INDEX: ScopeIndex = ScopeIndex::new(2);

/// Gets the `container` requirement from a requirements map.
pub(crate) fn container<'a>(
    requirements: &'a HashMap<String, Value>,
    default: Option<&'a str>,
) -> Cow<'a, str> {
    requirements
        .get(TASK_REQUIREMENT_CONTAINER)
        .or_else(|| requirements.get(TASK_REQUIREMENT_CONTAINER_ALIAS))
        .and_then(|v| -> Option<Cow<'_, str>> {
            // If the value is an array, use the first element or the default
            // Note: in the future we should be resolving which element in the array is
            // usable; this will require some work in Crankshaft to enable
            if let Some(array) = v.as_array() {
                return array.as_slice().first().map(|v| {
                    v.as_string()
                        .expect("type should be string")
                        .as_ref()
                        .into()
                });
            }

            Some(
                v.coerce(&PrimitiveType::String.into())
                    .expect("type should coerce")
                    .unwrap_string()
                    .as_ref()
                    .clone()
                    .into(),
            )
        })
        .and_then(|v| {
            // Treat star as the default
            if v == "*" { None } else { Some(v) }
        })
        .unwrap_or_else(|| {
            default
                .map(Into::into)
                .unwrap_or(DEFAULT_TASK_REQUIREMENT_CONTAINER.into())
        })
}

/// Gets the `cpu` requirement from a requirements map.
pub(crate) fn cpu(requirements: &HashMap<String, Value>) -> f64 {
    requirements
        .get(TASK_REQUIREMENT_CPU)
        .map(|v| {
            v.coerce(&PrimitiveType::Float.into())
                .expect("type should coerce")
                .unwrap_float()
        })
        .unwrap_or(DEFAULT_TASK_REQUIREMENT_CPU)
}

/// Gets the `max_cpu` hint from a hints map.
pub(crate) fn max_cpu(hints: &HashMap<String, Value>) -> Option<f64> {
    hints
        .get(TASK_HINT_MAX_CPU)
        .or_else(|| hints.get(TASK_HINT_MAX_CPU_ALIAS))
        .map(|v| {
            v.coerce(&PrimitiveType::Float.into())
                .expect("type should coerce")
                .unwrap_float()
        })
}

/// Gets the `memory` requirement from a requirements map.
pub(crate) fn memory(requirements: &HashMap<String, Value>) -> Result<i64> {
    Ok(requirements
        .get(TASK_REQUIREMENT_MEMORY)
        .map(|v| {
            if let Some(v) = v.as_integer() {
                return Ok(v);
            }

            if let Some(s) = v.as_string() {
                return convert_unit_string(s)
                    .and_then(|v| v.try_into().ok())
                    .with_context(|| {
                        format!("task specifies an invalid `memory` requirement `{s}`")
                    });
            }

            unreachable!("value should be an integer or string");
        })
        .transpose()?
        .unwrap_or(DEFAULT_TASK_REQUIREMENT_MEMORY))
}

/// Gets the `max_memory` hint from a hints map.
pub(crate) fn max_memory(hints: &HashMap<String, Value>) -> Result<Option<i64>> {
    hints
        .get(TASK_HINT_MAX_MEMORY)
        .or_else(|| hints.get(TASK_HINT_MAX_MEMORY_ALIAS))
        .map(|v| {
            if let Some(v) = v.as_integer() {
                return Ok(v);
            }

            if let Some(s) = v.as_string() {
                return convert_unit_string(s)
                    .and_then(|v| v.try_into().ok())
                    .with_context(|| {
                        format!("task specifies an invalid `memory` requirement `{s}`")
                    });
            }

            unreachable!("value should be an integer or string");
        })
        .transpose()
}

/// Used to evaluate expressions in tasks.
struct TaskEvaluationContext<'a, 'b> {
    /// The associated evaluation state.
    state: &'a State<'b>,
    /// The downloader to use for expression evaluation.
    downloader: &'a HttpDownloader,
    /// The current evaluation scope.
    scope: ScopeIndex,
    /// The work directory.
    ///
    /// This field is only available after task execution.
    work_dir: Option<&'a EvaluationPath>,
    /// The standard out value to use.
    ///
    /// This field is only available after task execution.
    stdout: Option<&'a Value>,
    /// The standard error value to use.
    ///
    /// This field is only available after task execution.
    stderr: Option<&'a Value>,
    /// The inputs for the evaluation.
    inputs: Option<&'a [Input]>,
    /// Whether or not the evaluation has associated task information.
    ///
    /// This is `true` when evaluating hints sections.
    task: bool,
}

impl<'a, 'b> TaskEvaluationContext<'a, 'b> {
    /// Constructs a new expression evaluation context.
    pub fn new(state: &'a State<'b>, downloader: &'a HttpDownloader, scope: ScopeIndex) -> Self {
        Self {
            state,
            downloader,
            scope,
            work_dir: None,
            stdout: None,
            stderr: None,
            inputs: None,
            task: false,
        }
    }

    /// Sets the working directory to use for the evaluation context.
    pub fn with_work_dir(mut self, work_dir: &'a EvaluationPath) -> Self {
        self.work_dir = Some(work_dir);
        self
    }

    /// Sets the stdout value to use for the evaluation context.
    pub fn with_stdout(mut self, stdout: &'a Value) -> Self {
        self.stdout = Some(stdout);
        self
    }

    /// Sets the stderr value to use for the evaluation context.
    pub fn with_stderr(mut self, stderr: &'a Value) -> Self {
        self.stderr = Some(stderr);
        self
    }

    /// Sets the inputs associated with the evaluation context.
    pub fn with_inputs(mut self, inputs: &'a [Input]) -> Self {
        self.inputs = Some(inputs);
        self
    }

    /// Marks the evaluation as having associated task information.
    ///
    /// This is used in evaluating hints sections.
    pub fn with_task(mut self) -> Self {
        self.task = true;
        self
    }
}

impl EvaluationContext for TaskEvaluationContext<'_, '_> {
    fn version(&self) -> SupportedVersion {
        self.state
            .document
            .version()
            .expect("document should have a version")
    }

    fn resolve_name(&self, name: &str, span: Span) -> Result<Value, Diagnostic> {
        ScopeRef::new(&self.state.scopes, self.scope)
            .lookup(name)
            .cloned()
            .ok_or_else(|| unknown_name(name, span))
    }

    fn resolve_type_name(&self, name: &str, span: Span) -> Result<Type, Diagnostic> {
        crate::resolve_type_name(self.state.document, name, span)
    }

    fn work_dir(&self) -> Option<&EvaluationPath> {
        self.work_dir
    }

    fn temp_dir(&self) -> &Path {
        self.state.root.temp_dir()
    }

    fn stdout(&self) -> Option<&Value> {
        self.stdout
    }

    fn stderr(&self) -> Option<&Value> {
        self.stderr
    }

    fn task(&self) -> Option<&Task> {
        if self.task {
            Some(self.state.task)
        } else {
            None
        }
    }

    fn translate_path(&self, path: &str) -> Option<Cow<'_, Path>> {
        let inputs = self.inputs?;
        let is_url = path::is_url(path);

        // We cannot translate a relative path
        if !is_url && Path::new(path).is_relative() {
            return None;
        }

        // The most specific (i.e. shortest stripped path) wins
        let (guest_path, stripped) = inputs
            .iter()
            .filter_map(|i| {
                match i.path() {
                    EvaluationPath::Local(base) if !is_url => {
                        let stripped = Path::new(path).strip_prefix(base).ok()?;
                        Some((i.guest_path()?, stripped.to_str()?))
                    }
                    EvaluationPath::Remote(url) if is_url => {
                        let url = url.as_str();
                        let stripped = path.strip_prefix(url.strip_suffix('/').unwrap_or(url))?;

                        // Strip off the query string or fragment
                        let stripped = if let Some(pos) = stripped.find('?') {
                            &stripped[..pos]
                        } else if let Some(pos) = stripped.find('#') {
                            &stripped[..pos]
                        } else {
                            stripped.strip_prefix('/').unwrap_or(stripped)
                        };

                        Some((i.guest_path()?, stripped))
                    }
                    _ => None,
                }
            })
            .min_by(|(_, a), (_, b)| a.len().cmp(&b.len()))?;

        if stripped.is_empty() {
            return Some(Path::new(guest_path).into());
        }

        Some(Path::new(guest_path).join(stripped).into())
    }

    fn downloader(&self) -> &dyn Downloader {
        self.downloader
    }
}

/// Represents task evaluation state.
struct State<'a> {
    /// The document containing the workflow being evaluated.
    document: &'a Document,
    /// The task being evaluated.
    task: &'a Task,
    /// The scopes of the task being evaluated.
    ///
    /// The first scope is the root scope, the second is the output scope, and
    /// the third is the scope where the "task" variable is visible in 1.2+
    /// evaluations.
    scopes: [Scope; 3],
    /// The execution root to spawn the task with.
    root: Arc<TaskExecutionRoot>,
    /// The environment variables of the task.
    ///
    /// Environment variables do not change between retries.
    env: IndexMap<String, String>,
}

impl<'a> State<'a> {
    /// Constructs a new task evaluation state.
    fn new(root: &Path, document: &'a Document, task: &'a Task) -> Result<Self> {
        // Tasks have a root scope (index 0), an output scope (index 1), and a `task`
        // variable scope (index 2). The output scope inherits from the root scope and
        // the task scope inherits from the output scope. Inputs and private
        // declarations are evaluated into the root scope. Outputs are evaluated into
        // the output scope. The task scope is used for evaluating expressions in both
        // the command and output sections. Only the `task` variable in WDL 1.2 is
        // introduced into the task scope; in previous WDL versions, the task scope will
        // not have any local names.
        let scopes = [
            Scope::default(),
            Scope::new(ROOT_SCOPE_INDEX),
            Scope::new(OUTPUT_SCOPE_INDEX),
        ];

        Ok(Self {
            document,
            task,
            scopes,
            root: Arc::new(TaskExecutionRoot::new(root, 0)?),
            env: Default::default(),
        })
    }

    /// Changes the root for a new attempt.
    fn set_root(&mut self, root: &Path, attempt: u64) -> Result<()> {
        self.root = Arc::new(TaskExecutionRoot::new(root, attempt)?);
        Ok(())
    }
}

/// Represents the result of evaluating task sections before execution.
struct EvaluatedSections {
    /// The evaluated command.
    command: String,
    /// The evaluated requirements.
    requirements: Arc<HashMap<String, Value>>,
    /// The evaluated hints.
    hints: Arc<HashMap<String, Value>>,
    /// The inputs to the task.
    inputs: Arc<[Input]>,
}

/// Represents a WDL V1 task evaluator.
pub struct TaskEvaluator {
    /// The associated evaluation configuration.
    config: Arc<Config>,
    /// The associated task execution backend.
    backend: Arc<dyn TaskExecutionBackend>,
    /// The cancellation token for cancelling task evaluation.
    token: CancellationToken,
    /// The downloader to use for expression evaluation.
    downloader: HttpDownloader,
}

impl TaskEvaluator {
    /// Constructs a new task evaluator with the given evaluation
    /// configuration and cancellation token.
    ///
    /// This method creates a default task execution backend.
    ///
    /// Returns an error if the configuration isn't valid.
    pub async fn new(config: Config, token: CancellationToken) -> Result<Self> {
        let backend = config.create_backend().await?;
        Self::new_with_backend(config, backend, token)
    }

    /// Constructs a new task evaluator with the given evaluation
    /// configuration, task execution backend, and cancellation token.
    ///
    /// Returns an error if the configuration isn't valid.
    pub fn new_with_backend(
        config: Config,
        backend: Arc<dyn TaskExecutionBackend>,
        token: CancellationToken,
    ) -> Result<Self> {
        config.validate()?;

        let config = Arc::new(config);
        let downloader = HttpDownloader::new(config.clone())?;

        Ok(Self {
            config,
            backend,
            token,
            downloader,
        })
    }

    /// Creates a new task evaluator with the given configuration, backend,
    /// cancellation token, and downloader.
    ///
    /// This method does not validate the configuration.
    pub(crate) fn new_unchecked(
        config: Arc<Config>,
        backend: Arc<dyn TaskExecutionBackend>,
        token: CancellationToken,
        downloader: HttpDownloader,
    ) -> Self {
        Self {
            config,
            backend,
            token,
            downloader,
        }
    }

    /// Evaluates the given task.
    ///
    /// Upon success, returns the evaluated task.
    pub async fn evaluate<P, R>(
        &self,
        document: &Document,
        task: &Task,
        inputs: &TaskInputs,
        root: impl AsRef<Path>,
        progress: P,
    ) -> EvaluationResult<EvaluatedTask>
    where
        P: Fn(ProgressKind<'_>) -> R + Send + Sync + 'static,
        R: Future<Output = ()> + Send,
    {
        let evaluated = self
            .evaluate_with_progress(
                document,
                task,
                inputs,
                root.as_ref(),
                task.name(),
                Arc::new(progress),
            )
            .await?;

        // Dumps are created for Local backend only
        if self.config.task.write_inputs && self.backend.guest_work_dir().is_none() {
            if let Err(e) = (|| -> anyhow::Result<()> {
                let mut wrapper = serde_json::Map::with_capacity(2);
                wrapper.insert(
                    "task".to_string(),
                    serde_json::Value::String(task.name().to_string()),
                );
                let mut inputs_map = serde_json::Map::new();
                for (k, v) in inputs.iter() {
                    let val =
                        serde_json::to_value(v).context("serializing individual task input")?;
                    inputs_map.insert(k.to_string().clone(), val);
                }
                wrapper.insert("inputs".to_string(), serde_json::Value::Object(inputs_map));

                let json_str = serde_json::to_string_pretty(&serde_json::Value::Object(wrapper))
                    .context("formatting inputs.json")?;

                let work_dir = evaluated
                    .work_dir()
                    .as_local()
                    .expect("local work dir for inputs.json");
                let file_path = work_dir.join("inputs.json");
                std::fs::write(&file_path, json_str).with_context(|| {
                    format!("writing debug inputs.json to {}", file_path.display())
                })?;
                Ok(())
            })() {
                tracing::warn!(error = %e, "debug‐dump of inputs.json failed");
            }
        }

        Ok(evaluated)
    }

    /// Evaluates the given task with the given shared progress callback.
    pub(crate) async fn evaluate_with_progress<P, R>(
        &self,
        document: &Document,
        task: &Task,
        inputs: &TaskInputs,
        root: &Path,
        id: &str,
        progress: Arc<P>,
    ) -> EvaluationResult<EvaluatedTask>
    where
        P: Fn(ProgressKind<'_>) -> R + Send + Sync + 'static,
        R: Future<Output = ()> + Send,
    {
        // We cannot evaluate a document with errors
        if document.has_errors() {
            return Err(anyhow!("cannot evaluate a document with errors").into());
        }

        progress(ProgressKind::TaskStarted { id }).await;

        let result = self
            .perform_evaluation(document, task, inputs, root, id, progress.clone())
            .await;

        progress(ProgressKind::TaskCompleted {
            id,
            result: &result,
        })
        .await;

        result
    }

    /// Performs the actual evaluation of the task.
    async fn perform_evaluation<P, R>(
        &self,
        document: &Document,
        task: &Task,
        inputs: &TaskInputs,
        root: &Path,
        id: &str,
        progress: Arc<P>,
    ) -> EvaluationResult<EvaluatedTask>
    where
        P: Fn(ProgressKind<'_>) -> R + Send + Sync + 'static,
        R: Future<Output = ()> + Send,
    {
        inputs.validate(document, task, None).with_context(|| {
            format!(
                "failed to validate the inputs to task `{task}`",
                task = task.name()
            )
        })?;

        let ast = match document.root().morph().ast() {
            Ast::V1(ast) => ast,
            _ => {
                return Err(
                    anyhow!("task evaluation is only supported for WDL 1.x documents").into(),
                );
            }
        };

        // Find the task in the AST
        let definition = ast
            .tasks()
            .find(|t| t.name().text() == task.name())
            .expect("task should exist in the AST");

        let version = document.version().expect("document should have version");

        // Build an evaluation graph for the task
        let mut diagnostics = Vec::new();
        let graph = TaskGraphBuilder::default().build(version, &definition, &mut diagnostics);
        assert!(
            diagnostics.is_empty(),
            "task evaluation graph should have no diagnostics"
        );

        info!(
            task_id = id,
            task_name = task.name(),
            document = document.uri().as_str(),
            "evaluating task"
        );

        let mut state = State::new(root, document, task)?;
        let nodes = toposort(&graph, None).expect("graph should be acyclic");
        let mut current = 0;
        while current < nodes.len() {
            match &graph[nodes[current]] {
                TaskGraphNode::Input(decl) => {
                    self.evaluate_input(id, &mut state, decl, inputs)
                        .await
                        .map_err(|d| EvaluationError::new(state.document.clone(), d))?;
                }
                TaskGraphNode::Decl(decl) => {
                    self.evaluate_decl(id, &mut state, decl)
                        .await
                        .map_err(|d| EvaluationError::new(state.document.clone(), d))?;
                }
                TaskGraphNode::Output(_) => {
                    // Stop at the first output
                    break;
                }
                TaskGraphNode::Command(_)
                | TaskGraphNode::Runtime(_)
                | TaskGraphNode::Requirements(_)
                | TaskGraphNode::Hints(_) => {
                    // Skip these sections for now; they'll evaluate in the
                    // retry loop
                }
            }

            current += 1;
        }

        // TODO: check call cache for a hit. if so, skip task execution and use cache
        // paths for output evaluation

        let env = Arc::new(mem::take(&mut state.env));

        // Spawn the task in a retry loop
        let mut attempt = 0;
        let (mut evaluated, mounts) = loop {
            let EvaluatedSections {
                command,
                requirements,
                hints,
                inputs,
            } = self
                .evaluate_sections(id, &mut state, &definition, inputs, attempt)
                .await?;

            // Get the maximum number of retries, either from the task's requirements or
            // from configuration
            let max_retries = requirements
                .get(TASK_REQUIREMENT_MAX_RETRIES)
                .or_else(|| requirements.get(TASK_REQUIREMENT_MAX_RETRIES_ALIAS))
                .cloned()
                .map(|v| v.unwrap_integer() as u64)
                .or_else(|| self.config.task.retries)
                .unwrap_or(DEFAULT_TASK_REQUIREMENT_MAX_RETRIES);

            if max_retries > MAX_RETRIES {
                return Err(anyhow!(
                    "task `max_retries` requirement of {max_retries} cannot exceed {MAX_RETRIES}"
                )
                .into());
            }

            let request = TaskSpawnRequest::new(
                state.root.clone(),
                id.to_string(),
                command,
                requirements.clone(),
                hints.clone(),
                env.clone(),
                inputs.clone(),
            );

            let events = self
                .backend
                .spawn(request, self.token.clone())
                .with_context(|| {
                    format!(
                        "failed to spawn task `{name}` in `{path}` (task id `{id}`)",
                        name = task.name(),
                        path = document.path(),
                    )
                })?;

            // Await the spawned notification first
            events.spawned.await.ok();

            progress(ProgressKind::TaskExecutionStarted { id, attempt });

            let result = events
                .completed
                .await
                .expect("failed to receive response from spawned task");

            progress(ProgressKind::TaskExecutionCompleted {
                id,
                result: &result,
            });

            let result = result.map_err(|e| {
                EvaluationError::new(
                    state.document.clone(),
                    task_execution_failed(e, task.name(), id, task.name_span()),
                )
            })?;

            // Update the task variable
            let evaluated = EvaluatedTask::new(state.root.clone(), result)?;
            if version >= SupportedVersion::V1(V1::Two) {
                let task = state.scopes[TASK_SCOPE_INDEX.0]
                    .get_mut(TASK_VAR_NAME)
                    .unwrap()
                    .as_task_mut()
                    .unwrap();

                task.set_attempt(attempt.try_into().with_context(|| {
                    format!(
                        "too many attempts were made to run task `{task}`",
                        task = state.task.name()
                    )
                })?);
                task.set_return_code(evaluated.exit_code);
            }

            if let Err(e) = evaluated.handle_exit(&requirements) {
                if attempt >= max_retries {
                    return Err(EvaluationError::new(
                        state.document.clone(),
                        task_execution_failed(e, task.name(), id, task.name_span()),
                    ));
                }

                attempt += 1;

                // Update the execution root for the next attempt
                state.set_root(root, attempt)?;

                info!(
                    "retrying execution of task `{name}` (retry {attempt})",
                    name = state.task.name()
                );
                continue;
            }

            break (evaluated, inputs);
        };

        // Evaluate the remaining inputs (unused), and decls, and outputs
        for index in &nodes[current..] {
            match &graph[*index] {
                TaskGraphNode::Decl(decl) => {
                    self.evaluate_decl(id, &mut state, decl)
                        .await
                        .map_err(|d| EvaluationError::new(state.document.clone(), d))?;
                }
                TaskGraphNode::Output(decl) => {
                    self.evaluate_output(id, &mut state, decl, &evaluated, &mounts)
                        .await
                        .map_err(|d| EvaluationError::new(state.document.clone(), d))?;
                }
                _ => {
                    unreachable!(
                        "only declarations and outputs should be evaluated after the command"
                    )
                }
            }
        }

        // Take the output scope and return it
        let mut outputs: Outputs = mem::take(&mut state.scopes[OUTPUT_SCOPE_INDEX.0]).into();
        drop(state);
        if let Some(section) = definition.output() {
            let indexes: HashMap<_, _> = section
                .declarations()
                .enumerate()
                .map(|(i, d)| (d.name().hashable(), i))
                .collect();
            outputs.sort_by(move |a, b| indexes[a].cmp(&indexes[b]))
        }

        evaluated.outputs = Ok(outputs);
        Ok(evaluated)
    }

    /// Evaluates a task input.
    async fn evaluate_input(
        &self,
        id: &str,
        state: &mut State<'_>,
        decl: &Decl<SyntaxNode>,
        inputs: &TaskInputs,
    ) -> Result<(), Diagnostic> {
        let name = decl.name();
        let decl_ty = decl.ty();
        let ty = crate::convert_ast_type_v1(state.document, &decl_ty)?;

        let (value, span) = match inputs.get(name.text()) {
            Some(input) => (input.clone(), name.span()),
            None => match decl.expr() {
                Some(expr) => {
                    debug!(
                        task_id = id,
                        task_name = state.task.name(),
                        document = state.document.uri().as_str(),
                        input_name = name.text(),
                        "evaluating input"
                    );

                    let mut evaluator = ExprEvaluator::new(TaskEvaluationContext::new(
                        state,
                        &self.downloader,
                        ROOT_SCOPE_INDEX,
                    ));
                    let value = evaluator.evaluate_expr(&expr).await?;
                    (value, expr.span())
                }
                _ => {
                    assert!(decl.ty().is_optional(), "type should be optional");
                    (Value::None, name.span())
                }
            },
        };

        let value = value
            .coerce(&ty)
            .map_err(|e| runtime_type_mismatch(e, &ty, name.span(), &value.ty(), span))?;
        state.scopes[ROOT_SCOPE_INDEX.0].insert(name.text(), value.clone());

        // Insert an environment variable, if it is one
        if decl.env().is_some() {
            state.env.insert(
                name.text().to_string(),
                value
                    .as_primitive()
                    .expect("value should be primitive")
                    .raw(None)
                    .to_string(),
            );
        }

        Ok(())
    }

    /// Evaluates a task private declaration.
    async fn evaluate_decl(
        &self,
        id: &str,
        state: &mut State<'_>,
        decl: &Decl<SyntaxNode>,
    ) -> Result<(), Diagnostic> {
        let name = decl.name();
        debug!(
            task_id = id,
            task_name = state.task.name(),
            document = state.document.uri().as_str(),
            decl_name = name.text(),
            "evaluating private declaration",
        );

        let decl_ty = decl.ty();
        let ty = crate::convert_ast_type_v1(state.document, &decl_ty)?;

        let mut evaluator = ExprEvaluator::new(TaskEvaluationContext::new(
            state,
            &self.downloader,
            ROOT_SCOPE_INDEX,
        ));

        let expr = decl.expr().expect("private decls should have expressions");
        let value = evaluator.evaluate_expr(&expr).await?;
        let value = value
            .coerce(&ty)
            .map_err(|e| runtime_type_mismatch(e, &ty, name.span(), &value.ty(), expr.span()))?;
        state.scopes[ROOT_SCOPE_INDEX.0].insert(name.text(), value.clone());

        // Insert an environment variable, if it is one
        if decl.env().is_some() {
            state.env.insert(
                name.text().to_string(),
                value
                    .as_primitive()
                    .expect("value should be primitive")
                    .raw(None)
                    .to_string(),
            );
        }

        Ok(())
    }

    /// Evaluates the runtime section.
    ///
    /// Returns both the task's hints and requirements.
    async fn evaluate_runtime_section(
        &self,
        id: &str,
        state: &State<'_>,
        section: &RuntimeSection<SyntaxNode>,
        inputs: &TaskInputs,
    ) -> Result<(HashMap<String, Value>, HashMap<String, Value>), Diagnostic> {
        debug!(
            task_id = id,
            task_name = state.task.name(),
            document = state.document.uri().as_str(),
            "evaluating runtimes section",
        );

        let mut requirements = HashMap::new();
        let mut hints = HashMap::new();

        let version = state
            .document
            .version()
            .expect("document should have version");
        for item in section.items() {
            let name = item.name();
            match inputs.requirement(name.text()) {
                Some(value) => {
                    requirements.insert(name.text().to_string(), value.clone());
                    continue;
                }
                _ => {
                    if let Some(value) = inputs.hint(name.text()) {
                        hints.insert(name.text().to_string(), value.clone());
                        continue;
                    }
                }
            }

            let mut evaluator = ExprEvaluator::new(TaskEvaluationContext::new(
                state,
                &self.downloader,
                ROOT_SCOPE_INDEX,
            ));

            let (types, requirement) = match task_requirement_types(version, name.text()) {
                Some(types) => (Some(types), true),
                None => match task_hint_types(version, name.text(), false) {
                    Some(types) => (Some(types), false),
                    None => (None, false),
                },
            };

            // Evaluate and coerce to the expected type
            let expr = item.expr();
            let mut value = evaluator.evaluate_expr(&expr).await?;
            if let Some(types) = types {
                value = types
                    .iter()
                    .find_map(|ty| value.coerce(ty).ok())
                    .ok_or_else(|| {
                        multiple_type_mismatch(types, name.span(), &value.ty(), expr.span())
                    })?;
            }

            if requirement {
                requirements.insert(name.text().to_string(), value);
            } else {
                hints.insert(name.text().to_string(), value);
            }
        }

        Ok((requirements, hints))
    }

    /// Evaluates the requirements section.
    async fn evaluate_requirements_section(
        &self,
        id: &str,
        state: &State<'_>,
        section: &RequirementsSection<SyntaxNode>,
        inputs: &TaskInputs,
    ) -> Result<HashMap<String, Value>, Diagnostic> {
        debug!(
            task_id = id,
            task_name = state.task.name(),
            document = state.document.uri().as_str(),
            "evaluating requirements",
        );

        let mut requirements = HashMap::new();

        let version = state
            .document
            .version()
            .expect("document should have version");
        for item in section.items() {
            let name = item.name();
            if let Some(value) = inputs.requirement(name.text()) {
                requirements.insert(name.text().to_string(), value.clone());
                continue;
            }

            let mut evaluator = ExprEvaluator::new(TaskEvaluationContext::new(
                state,
                &self.downloader,
                ROOT_SCOPE_INDEX,
            ));

            let types =
                task_requirement_types(version, name.text()).expect("requirement should be known");

            // Evaluate and coerce to the expected type
            let expr = item.expr();
            let value = evaluator.evaluate_expr(&expr).await?;
            let value = types
                .iter()
                .find_map(|ty| value.coerce(ty).ok())
                .ok_or_else(|| {
                    multiple_type_mismatch(types, name.span(), &value.ty(), expr.span())
                })?;

            requirements.insert(name.text().to_string(), value);
        }

        Ok(requirements)
    }

    /// Evaluates the hints section.
    async fn evaluate_hints_section(
        &self,
        id: &str,
        state: &State<'_>,
        section: &TaskHintsSection<SyntaxNode>,
        inputs: &TaskInputs,
    ) -> Result<HashMap<String, Value>, Diagnostic> {
        debug!(
            task_id = id,
            task_name = state.task.name(),
            document = state.document.uri().as_str(),
            "evaluating hints section",
        );

        let mut hints = HashMap::new();

        for item in section.items() {
            let name = item.name();
            if let Some(value) = inputs.hint(name.text()) {
                hints.insert(name.text().to_string(), value.clone());
                continue;
            }

            let mut evaluator = ExprEvaluator::new(
                TaskEvaluationContext::new(state, &self.downloader, ROOT_SCOPE_INDEX).with_task(),
            );

            let value = evaluator.evaluate_hints_item(&name, &item.expr()).await?;
            hints.insert(name.text().to_string(), value);
        }

        Ok(hints)
    }

    /// Evaluates the command of a task.
    ///
    /// Returns the evaluated command and the mounts to use for spawning the
    /// task.
    async fn evaluate_command(
        &self,
        id: &str,
        state: &State<'_>,
        section: &CommandSection<SyntaxNode>,
    ) -> EvaluationResult<(String, Vec<Input>)> {
        debug!(
            task_id = id,
            task_name = state.task.name(),
            document = state.document.uri().as_str(),
            "evaluating command section",
        );

        // Determine the inputs to the task
        let mut inputs = Vec::new();

        // Discover every input that's visible to the scope
        ScopeRef::new(&state.scopes, TASK_SCOPE_INDEX.0).for_each(|_, v| {
            v.visit_paths(false, &mut |_, value| {
                inputs.push(Input::new(EvaluationPath::from_primitive_value(value)?));
                Ok(())
            })
        })?;

        // The temp directory should always be an input
        inputs.push(Input::new(EvaluationPath::Local(
            state.root.temp_dir().to_path_buf(),
        )));

        // Localize the inputs
        self.backend
            .localize_inputs(&self.downloader, &mut inputs)
            .await
            .map_err(|e| {
                EvaluationError::new(
                    state.document.clone(),
                    task_localization_failed(e, state.task.name(), state.task.name_span()),
                )
            })?;

        if enabled!(Level::DEBUG) {
            for input in inputs.iter() {
                if let Some(location) = input.location() {
                    debug!(
                        task_id = id,
                        task_name = state.task.name(),
                        document = state.document.uri().as_str(),
                        "task input `{path}` (downloaded to `{location}`) mapped to `{guest_path}`",
                        path = input.path().display(),
                        location = location.display(),
                        guest_path = input.guest_path().unwrap_or(""),
                    );
                } else {
                    debug!(
                        task_id = id,
                        task_name = state.task.name(),
                        document = state.document.uri().as_str(),
                        "task input `{path}` mapped to `{guest_path}`",
                        path = input.path().display(),
                        guest_path = input.guest_path().unwrap_or(""),
                    );
                }
            }
        }

        let mut command = String::new();
        match section.strip_whitespace() {
            Some(parts) => {
                let mut evaluator = ExprEvaluator::new(
                    TaskEvaluationContext::new(state, &self.downloader, TASK_SCOPE_INDEX)
                        .with_inputs(&inputs),
                );

                for part in parts {
                    match part {
                        StrippedCommandPart::Text(t) => {
                            command.push_str(t.as_str());
                        }
                        StrippedCommandPart::Placeholder(placeholder) => {
                            evaluator
                                .evaluate_placeholder(&placeholder, &mut command)
                                .await
                                .map_err(|d| EvaluationError::new(state.document.clone(), d))?;
                        }
                    }
                }
            }
            _ => {
                warn!(
                    "command for task `{task}` in `{uri}` has mixed indentation; whitespace \
                     stripping was skipped",
                    task = state.task.name(),
                    uri = state.document.uri(),
                );

                let mut evaluator = ExprEvaluator::new(
                    TaskEvaluationContext::new(state, &self.downloader, TASK_SCOPE_INDEX)
                        .with_inputs(&inputs),
                );

                let heredoc = section.is_heredoc();
                for part in section.parts() {
                    match part {
                        CommandPart::Text(t) => {
                            t.unescape_to(heredoc, &mut command);
                        }
                        CommandPart::Placeholder(placeholder) => {
                            evaluator
                                .evaluate_placeholder(&placeholder, &mut command)
                                .await
                                .map_err(|d| EvaluationError::new(state.document.clone(), d))?;
                        }
                    }
                }
            }
        }

        Ok((command, inputs))
    }

    /// Evaluates sections prior to spawning the command.
    ///
    /// This method evaluates the following sections:
    ///   * runtime
    ///   * requirements
    ///   * hints
    ///   * command
    async fn evaluate_sections(
        &self,
        id: &str,
        state: &mut State<'_>,
        definition: &TaskDefinition<SyntaxNode>,
        inputs: &TaskInputs,
        attempt: u64,
    ) -> EvaluationResult<EvaluatedSections> {
        // Start by evaluating requirements and hints
        let (requirements, hints) = match definition.runtime() {
            Some(section) => self
                .evaluate_runtime_section(id, state, &section, inputs)
                .await
                .map_err(|d| EvaluationError::new(state.document.clone(), d))?,
            _ => (
                match definition.requirements() {
                    Some(section) => self
                        .evaluate_requirements_section(id, state, &section, inputs)
                        .await
                        .map_err(|d| EvaluationError::new(state.document.clone(), d))?,
                    None => Default::default(),
                },
                match definition.hints() {
                    Some(section) => self
                        .evaluate_hints_section(id, state, &section, inputs)
                        .await
                        .map_err(|d| EvaluationError::new(state.document.clone(), d))?,
                    None => Default::default(),
                },
            ),
        };

        // Update or insert the `task` variable in the task scope
        // TODO: if task variables become visible in `requirements` or `hints`  section,
        // this needs to be relocated to before we evaluate those sections
        if state.document.version() >= Some(SupportedVersion::V1(V1::Two)) {
            // Get the execution constraints
            let constraints = self
                .backend
                .constraints(&requirements, &hints)
                .with_context(|| {
                    format!(
                        "failed to get constraints for task `{task}`",
                        task = state.task.name()
                    )
                })?;

            let task = TaskValue::new_v1(
                state.task.name(),
                id,
                definition,
                constraints,
                attempt.try_into().with_context(|| {
                    format!(
                        "too many attempts were made to run task `{task}`",
                        task = state.task.name()
                    )
                })?,
            );

            let scope = &mut state.scopes[TASK_SCOPE_INDEX.0];
            if let Some(v) = scope.get_mut(TASK_VAR_NAME) {
                *v = Value::Task(task);
            } else {
                scope.insert(TASK_VAR_NAME, Value::Task(task));
            }
        }

        let (command, inputs) = self
            .evaluate_command(
                id,
                state,
                &definition.command().expect("must have command section"),
            )
            .await?;

        Ok(EvaluatedSections {
            command,
            requirements: Arc::new(requirements),
            hints: Arc::new(hints),
            inputs: inputs.into(),
        })
    }

    /// Evaluates a task output.
    async fn evaluate_output(
        &self,
        id: &str,
        state: &mut State<'_>,
        decl: &Decl<SyntaxNode>,
        evaluated: &EvaluatedTask,
        inputs: &[Input],
    ) -> Result<(), Diagnostic> {
        let name = decl.name();
        debug!(
            task_id = id,
            task_name = state.task.name(),
            document = state.document.uri().as_str(),
            output_name = name.text(),
            "evaluating output",
        );

        let decl_ty = decl.ty();
        let ty = crate::convert_ast_type_v1(state.document, &decl_ty)?;
        let mut evaluator = ExprEvaluator::new(
            TaskEvaluationContext::new(state, &self.downloader, TASK_SCOPE_INDEX)
                .with_work_dir(&evaluated.work_dir)
                .with_stdout(&evaluated.stdout)
                .with_stderr(&evaluated.stderr),
        );

        let expr = decl.expr().expect("outputs should have expressions");
        let value = evaluator.evaluate_expr(&expr).await?;

        // First coerce the output value to the expected type
        let mut value = value
            .coerce(&ty)
            .map_err(|e| runtime_type_mismatch(e, &ty, name.span(), &value.ty(), expr.span()))?;

        let result = if let Some(guest_work_dir) = self.backend.guest_work_dir() {
            // Perform guest to host path translation and check for existence
            value.visit_paths_mut(ty.is_optional(), &mut |optional, value| {
                let path = match value {
                    PrimitiveValue::File(path) => path,
                    PrimitiveValue::Directory(path) => path,
                    _ => unreachable!("only file and directory values should be visited"),
                };

                // If the path isn't already within the host root, perform translation
                if !Path::new(path.as_str()).starts_with(state.root.path()) {
                    // It's a file scheme'd URL, treat it as an absolute guest path
                    let guest = if path::is_file_url(path) {
                        path::parse_url(path)
                            .and_then(|u| u.to_file_path().ok())
                            .ok_or_else(|| anyhow!("guest path `{path}` is not a valid file URI"))?
                    } else if path::is_url(path) {
                        // Treat other URLs as if they exist
                        // TODO: should probably issue a HEAD request to verify
                        return Ok(true);
                    } else {
                        // Otherwise, treat as relative to the guest working directory
                        guest_work_dir.join(path.as_str())
                    };

                    // If the path is inside of the working directory, join with the task's working
                    // directory
                    let host = if let Ok(stripped) = guest.strip_prefix(guest_work_dir) {
                        Cow::Owned(
                            evaluated.work_dir.join(
                                stripped.to_str().with_context(|| {
                                    format!("output path `{path}` is not UTF-8")
                                })?,
                            )?,
                        )
                    } else {
                        inputs
                            .iter()
                            .filter_map(|i| {
                                Some((i.path(), guest.strip_prefix(i.guest_path()?).ok()?))
                            })
                            .min_by(|(_, a), (_, b)| a.as_os_str().len().cmp(&b.as_os_str().len()))
                            .and_then(|(path, stripped)| {
                                if stripped.as_os_str().is_empty() {
                                    return Some(Cow::Borrowed(path));
                                }

                                Some(Cow::Owned(path.join(stripped.to_str()?).ok()?))
                            })
                            .ok_or_else(|| {
                                anyhow!("guest path `{path}` is not within a container mount")
                            })?
                    };

                    // Update the value to the host path
                    *Arc::make_mut(path) = host.into_owned().try_into()?;
                }

                // Finally, ensure the value exists
                value.ensure_path_exists(optional)
            })
        } else {
            // Backend isn't containerized, just join host paths and check for existence
            value.visit_paths_mut(ty.is_optional(), &mut |optional, value| {
                if let Some(work_dir) = evaluated.work_dir.as_local() {
                    value.join_path_to(work_dir);
                }

                value.ensure_path_exists(optional)
            })
        };

        result.map_err(|e| {
            output_evaluation_failed(e, state.task.name(), true, name.text(), name.span())
        })?;

        state.scopes[OUTPUT_SCOPE_INDEX.0].insert(name.text(), value);
        Ok(())
    }
}
