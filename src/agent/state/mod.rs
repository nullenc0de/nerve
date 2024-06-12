use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

use anyhow::Result;
use colored::Colorize;
use history::{Execution, History};
use storage::Storage;

use super::{
    actions::{self, Namespace},
    generator::Message,
    parsing::Invocation,
    task::Task,
};

mod history;
pub(crate) mod storage;

#[derive(Debug)]
pub struct State {
    // the task
    task: Box<dyn Task>,
    // current iteration and max
    curr_iter: usize,
    max_iters: usize,
    // model memories, goals and other storages
    storages: HashMap<String, Storage>,
    // available actions and execution history
    namespaces: Vec<Namespace>,
    // list of executed actions
    history: Mutex<History>,
    // set to true when task is complete
    complete: AtomicBool,
}

impl State {
    pub fn new(task: Box<dyn Task>, max_iterations: usize) -> Result<Self> {
        let complete = AtomicBool::new(false);
        let mut storages = HashMap::new();
        let history = Mutex::new(History::new());

        let mut namespaces = vec![];
        let using = task.namespaces();

        if let Some(using) = using {
            // add only task defined namespaces
            for (name, build_namespace) in &*actions::NAMESPACES {
                if using.contains(name) {
                    namespaces.push(build_namespace());
                }
            }
        } else {
            // add all available namespaces
            for build_namespace in actions::NAMESPACES.values() {
                namespaces.push(build_namespace());
            }
        }

        // add task defined actions
        namespaces.append(&mut task.get_functions());

        // if any namespace requires a specific storage, create it
        for namespace in &namespaces {
            if let Some(ns_storages) = &namespace.storages {
                for storage in ns_storages {
                    // not created yet
                    if !storages.contains_key(&storage.name) {
                        storages.insert(
                            storage.name.to_string(),
                            Storage::new(&storage.name, storage.type_.clone()),
                        );
                    }
                }
            }
        }

        // println!("storages={:?}", &storages);

        // if the goal namespace is enabled, set the current goal
        if let Some(goal) = storages.get("goal") {
            let prompt = task.to_prompt()?;
            goal.set_current(&prompt, false);
        }

        Ok(Self {
            task,
            storages,
            history,
            namespaces,
            complete,
            max_iters: max_iterations,
            curr_iter: 0,
        })
    }

    pub fn on_next_iteration(&mut self) -> Result<()> {
        self.curr_iter += 1;
        if self.max_iters > 0 && self.curr_iter >= self.max_iters {
            Err(anyhow!("maximum number of iterations reached"))
        } else {
            Ok(())
        }
    }

    pub fn to_chat_history(&self, max: usize) -> Result<Vec<Message>> {
        self.history.lock().unwrap().to_chat_history(max)
    }

    pub fn get_storage(&self, name: &str) -> Result<&Storage> {
        if let Some(storage) = self.storages.get(name) {
            Ok(storage)
        } else {
            println!("WARNING: requested storage '{name}' not found.");
            Err(anyhow!("storage {name} not found"))
        }
    }

    pub(crate) fn available_actions_to_string(&self) -> Result<String> {
        let mut md = "".to_string();

        for group in &self.namespaces {
            md += &format!("## {}\n\n", group.name);
            if !group.description.is_empty() {
                md += &format!("{}\n\n", group.description);
            }
            for action in &group.actions {
                md += &format!(
                    "{}\n{}\n\n",
                    action.description(),
                    action.structured_example()
                );
            }
        }

        Ok(md)
    }

    pub fn to_pretty_string(&self) -> Result<String> {
        let iterations = if self.max_iters > 0 {
            format!(
                "You are currently at step {} of a maximum of {}.\n",
                self.curr_iter + 1,
                self.max_iters
            )
        } else {
            "".to_string()
        };
        let mut storages = vec![];

        let mut sorted: Vec<&Storage> = self.storages.values().collect();

        sorted.sort_by_key(|x| x.get_type_int());

        for storage in sorted {
            storages.push(storage.to_structured_string());
        }

        let storages = storages.join("\n");

        Ok(format!("{storages}\n{iterations}"))
    }

    pub fn to_system_prompt(&self) -> Result<String> {
        let system_prompt = self.task.to_system_prompt()?;
        let mut storages = vec![];

        for storage in self.storages.values() {
            storages.push(storage.to_structured_string());
        }

        let storages = storages.join("\n\n");
        let guidance = self
            .task
            .guidance()?
            .into_iter()
            .map(|s| format!("- {}", s))
            .collect::<Vec<String>>()
            .join("\n");
        let available_actions = self.available_actions_to_string()?;

        let iterations = if self.max_iters > 0 {
            format!(
                "You are currently at step {} of a maximum of {}.",
                self.curr_iter + 1,
                self.max_iters
            )
        } else {
            "".to_string()
        };

        Ok(format!(
            include_str!("system_prompt.tpl"),
            iterations = iterations,
            system_prompt = system_prompt,
            storages = storages,
            available_actions = available_actions,
            guidance = guidance,
        ))
    }

    pub fn to_prompt(&self) -> Result<String> {
        self.task.to_prompt()
    }

    fn add_execution_to_history(
        &self,
        invocation: Invocation,
        result: Option<String>,
        error: Option<String>,
    ) {
        if let Ok(mut guard) = self.history.lock() {
            guard.push(Execution::new(invocation, result, error));
        }
    }

    pub async fn execute(&self, invocation: Invocation) -> Result<()> {
        // println!("[INVOKE]");

        for group in &self.namespaces {
            for action in &group.actions {
                if invocation.action == action.name() {
                    let inv = invocation.clone();

                    // check if valid payload has been provided
                    if let Some(payload) = invocation.payload.as_ref() {
                        if action.example_payload().unwrap() == payload {
                            self.add_execution_to_history(inv, None, Some("do not use the example values but use the information you have to create new ones".to_string()));
                            return Ok(());
                        }
                    }

                    // check if valid attributes have been provided
                    if let Some(attrs) = invocation.attributes.as_ref() {
                        if action.attributes().as_ref().unwrap() == attrs {
                            self.add_execution_to_history(inv, None, Some("do not use the example values but use the information you have to create new ones".to_string()));
                            return Ok(());
                        }
                    }

                    // execute the action
                    let ret = action.run(self, invocation.attributes, invocation.payload);

                    if let Err(error) = ret {
                        self.add_execution_to_history(inv, None, Some(error.to_string()));
                    } else {
                        self.add_execution_to_history(inv, ret.unwrap(), None);
                    }

                    return Ok(());
                }
            }
        }

        /*
        Err(anyhow!(
            "action '{}' not available: {:?}",
            &invocation.action,
            &invocation.xml
        ))
         */
        Ok(())
    }

    pub fn on_complete(&self, impossible: bool, reason: Option<String>) -> Result<()> {
        if impossible {
            println!(
                "\n{}: '{}'",
                "task is impossible".bold().red(),
                if let Some(r) = &reason {
                    r
                } else {
                    "no reason provided"
                }
            );
        } else {
            println!(
                "\n{}: '{}'",
                "task complete".bold().green(),
                if let Some(r) = &reason {
                    r
                } else {
                    "no reason provided"
                }
            );
        }

        self.complete.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub fn is_complete(&self) -> bool {
        self.complete.load(Ordering::SeqCst)
    }

    pub fn used_namespaces(&self) -> Vec<String> {
        self.namespaces
            .iter()
            .map(|n| n.name.to_string().to_lowercase())
            .collect()
    }
}
