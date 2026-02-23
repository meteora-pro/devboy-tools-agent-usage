#![allow(dead_code)]

mod activity;
mod classification;
mod claude;
mod cli;
mod config;
mod correlation;
mod output;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Commands};
use config::Config;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::detect()?;

    match cli.command {
        Commands::Projects { format } => {
            output::commands::projects(&config, &format)?;
        }
        Commands::Sessions {
            project,
            from,
            to,
            limit,
            format,
        } => {
            output::commands::sessions(
                &config,
                project.as_deref(),
                from.as_deref(),
                to.as_deref(),
                limit,
                &format,
            )?;
        }
        Commands::Summary {
            project,
            from,
            to,
            format,
        } => {
            output::commands::summary(
                &config,
                project.as_deref(),
                from.as_deref(),
                to.as_deref(),
                &format,
            )?;
        }
        Commands::Session {
            session_id,
            correlate,
            with_llm,
            format,
        } => {
            output::commands::session(&config, &session_id, correlate, with_llm, &format)?;
        }
        Commands::Focus {
            project,
            from,
            to,
            format,
        } => {
            output::commands::focus(
                &config,
                project.as_deref(),
                from.as_deref(),
                to.as_deref(),
                &format,
            )?;
        }
        Commands::Timeline { id } => {
            output::commands::timeline(&config, &id)?;
        }
        Commands::Browse { session_id, format } => {
            output::commands::browse(&config, &session_id, &format)?;
        }
        Commands::Tasks {
            project,
            from,
            to,
            with_aw,
            with_llm,
            sort,
            format,
        } => {
            let classifier = if with_llm {
                match classification::Classifier::new() {
                    Ok(c) => Some(c),
                    Err(e) => {
                        eprintln!("Warning: failed to initialize classifier: {}", e);
                        None
                    }
                }
            } else {
                None
            };
            output::commands::tasks(
                &config,
                project.as_deref(),
                from.as_deref(),
                to.as_deref(),
                with_aw,
                classifier.as_ref(),
                &sort,
                &format,
            )?;
        }
        Commands::Reclassify { from, to, project } => {
            output::commands::reclassify(
                &config,
                project.as_deref(),
                from.as_deref(),
                to.as_deref(),
            )?;
        }
        Commands::Retitle { task_id, title } => {
            output::commands::retitle(&task_id, &title)?;
        }
        Commands::Cost {
            project,
            from,
            to,
            group_by,
            format,
        } => {
            output::commands::cost(
                &config,
                project.as_deref(),
                from.as_deref(),
                to.as_deref(),
                &group_by,
                &format,
            )?;
        }
    }

    Ok(())
}
