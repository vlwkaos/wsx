//! `wsx task`: start a headless worker and steer, read and close it.
//! ref: docs/tasks.md

mod claude;
pub mod host;

use anyhow::Result;
use clap::Subcommand;
use wsx_core::task::store;

#[derive(Subcommand)]
pub enum TaskCmd {
    #[command(hide = true)]
    Host { project_id: String, task: String },
}

impl TaskCmd {
    pub fn json_output(&self) -> bool {
        false
    }

    /// Commands that read files or a host socket and never need wsxd.
    pub fn without_daemon(&self) -> bool {
        true
    }
}

pub fn run(command: TaskCmd) -> Result<()> {
    match command {
        TaskCmd::Host { project_id, task } => {
            host::run(&project_id, store::parse_task_number(&task)?)
        }
    }
}
