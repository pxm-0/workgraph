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

/// One approved check the kernel itself ran, at a tree.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckResult {
    name: String,
    command: String,
    exit_code: i64,
    output_tail: String,
    tree: String,
    nonce: String,
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
        }
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
