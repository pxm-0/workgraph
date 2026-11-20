//! Kernel capability tokens (design §4.1): private fields, no `Deserialize`, constructors visible
//! only inside the kernel crate. A factory or an LLM that invents a check result gains nothing:
//! finalize looks only at these.
//!
//! None can be built outside the kernel:
//!
//! ```compile_fail
//! let _ = wg::tokens::CheckResult { name: String::new(), command: String::new(), exit_code: 0,
//!     output_tail: String::new(), tree: String::new(), nonce: String::new() };
//! ```
//!
//! ```compile_fail
//! let _ = wg::tokens::ReviewPass { task: String::new(), tree: String::new(), label: String::new() };
//! ```
//!
//! ```compile_fail
//! let _ = wg::tokens::HumanApproval { task: String::new(), head: String::new(), by: String::new() };
//! ```
//!
//! And none can be deserialized:
//!
//! ```compile_fail
//! let _: wg::tokens::CheckResult = serde_json::from_str("{}").unwrap();
//! ```
//!
//! ```compile_fail
//! let _: wg::tokens::ReviewPass = serde_json::from_str("{}").unwrap();
//! ```
//!
//! ```compile_fail
//! let _ = wg::tokens::SandboxRun { job: String::new(), nonce: String::new(), label: String::new(),
//!     tree: String::new(), image: String::new(), steps: vec![], clean: true };
//! ```
//!
//! ```compile_fail
//! let _: wg::tokens::SandboxRun = serde_json::from_str("{}").unwrap();
//! ```

/// Where a check ran: the repo's dev stack on the Mac, or the sim sandbox (design §8.5 step 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Host,
    SimSandbox,
}

impl Target {
    pub fn name(self) -> &'static str {
        match self {
            Target::Host => "host",
            Target::SimSandbox => "sim_sandbox",
        }
    }
}

/// One approved check the kernel itself ran, at a tree.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckResult {
    name: String,
    command: String,
    exit_code: i64,
    output_tail: String,
    tree: String,
    nonce: String,
    target: Target,
}

impl CheckResult {
    pub(crate) fn new(
        name: &str,
        command: &str,
        exit_code: i64,
        output_tail: &str,
        tree: &str,
        nonce: &str,
    ) -> CheckResult {
        CheckResult {
            name: name.into(),
            command: command.into(),
            exit_code,
            output_tail: output_tail.into(),
            tree: tree.into(),
            nonce: nonce.into(),
            target: Target::Host,
        }
    }

    /// The same check, run in the sim sandbox and checked against the job sent.
    pub(crate) fn in_sandbox(
        name: &str,
        command: &str,
        exit_code: i64,
        output_tail: &str,
        tree: &str,
        nonce: &str,
    ) -> CheckResult {
        CheckResult {
            target: Target::SimSandbox,
            ..CheckResult::new(name, command, exit_code, output_tail, tree, nonce)
        }
    }

    pub fn target(&self) -> Target {
        self.target
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn command(&self) -> &str {
        &self.command
    }
    pub fn exit_code(&self) -> i64 {
        self.exit_code
    }
    pub fn output_tail(&self) -> &str {
        &self.output_tail
    }
    pub fn tree(&self) -> &str {
        &self.tree
    }
    pub fn nonce(&self) -> &str {
        &self.nonce
    }
    pub fn passed(&self) -> bool {
        self.exit_code == 0
    }
}

/// An opus reviewer that passed the change, at a tree.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewPass {
    task: String,
    tree: String,
    label: String,
}

impl ReviewPass {
    pub(crate) fn new(task: &str, tree: &str, label: &str) -> ReviewPass {
        ReviewPass {
            task: task.into(),
            tree: tree.into(),
            label: label.into(),
        }
    }
    pub fn task(&self) -> &str {
        &self.task
    }
    pub fn tree(&self) -> &str {
        &self.tree
    }
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// The owner's yes, made only from the command line of `wg finalize`.
#[derive(Clone, Debug, PartialEq)]
pub struct HumanApproval {
    task: String,
    head: String,
    by: String,
}

impl HumanApproval {
    pub(crate) fn new(task: &str, head: &str, by: &str) -> HumanApproval {
        HumanApproval {
            task: task.into(),
            head: head.into(),
            by: by.into(),
        }
    }
    pub fn task(&self) -> &str {
        &self.task
    }
    pub fn head(&self) -> &str {
        &self.head
    }
    pub fn by(&self) -> &str {
        &self.by
    }
}

/// A sandbox job the kernel sent and whose answer matched it (job id, nonce, steps, sizes): the
/// evidence a risky unit's PR carries (design §8.1, §8.5 step 7).
#[derive(Clone, Debug, PartialEq)]
pub struct SandboxRun {
    job: String,
    nonce: String,
    label: String,
    tree: String,
    image: String,
    steps: Vec<(String, i64)>,
    clean: bool,
}

impl SandboxRun {
    pub(crate) fn new(
        r: &crate::sandbox::job::JobResult,
        label: &str,
        tree: &str,
        image: &str,
    ) -> SandboxRun {
        SandboxRun {
            job: r.job.clone(),
            nonce: r.nonce.clone(),
            label: label.into(),
            tree: tree.into(),
            image: image.into(),
            steps: r.steps.iter().map(|s| (s.name.clone(), s.exit)).collect(),
            clean: r.cleanup.containers_left == 0 && r.cleanup.volumes_left == 0,
        }
    }

    pub(crate) fn restore(
        job: &str,
        nonce: &str,
        label: &str,
        tree: &str,
        image: &str,
        steps: Vec<(String, i64)>,
        clean: bool,
    ) -> SandboxRun {
        SandboxRun {
            job: job.into(),
            nonce: nonce.into(),
            label: label.into(),
            tree: tree.into(),
            image: image.into(),
            steps,
            clean,
        }
    }

    pub fn job(&self) -> &str {
        &self.job
    }
    pub fn nonce(&self) -> &str {
        &self.nonce
    }
    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn tree(&self) -> &str {
        &self.tree
    }
    pub fn image(&self) -> &str {
        &self.image
    }
    pub fn steps(&self) -> &[(String, i64)] {
        &self.steps
    }
    pub fn clean(&self) -> bool {
        self.clean
    }
}
