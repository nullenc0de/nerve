use regex::Regex;
use std::collections::HashMap;

use anyhow::Result;
use ollama_rs::{
    generation::{completion::request::GenerationRequest, options::GenerationOptions},
    Ollama,
};
use state::State;
use task::Task;

pub mod actions;
mod history;
mod memory;
pub mod state;
pub mod task;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Invocation {
    pub action: String,
    pub attributes: Option<HashMap<String, String>>,
    pub payload: Option<String>,

    xml: String,
}

impl Invocation {
    pub fn new(
        action: String,
        attributes: Option<HashMap<String, String>>,
        payload: Option<String>,
    ) -> Self {
        let mut xml = format!("<{action}");
        if let Some(attrs) = &attributes {
            for (key, value) in attrs {
                xml += &format!(" {key}=\"{value}\"");
            }
        }
        xml += &format!(
            ">{}</{}>",
            if let Some(data) = &payload { data } else { "" },
            action
        );

        Self {
            action,
            attributes,
            payload,
            xml,
        }
    }

    pub fn to_structured_string(&self) -> &str {
        return self.xml.as_str();
    }
}

pub struct Agent {
    ollama: Ollama,
    model_name: String,
    persist_prompt_path: Option<String>,
    persist_state_path: Option<String>,
    state: State,
}

impl Agent {
    pub fn new(
        ollama: Ollama,
        model_name: String,
        task: Box<dyn Task>,
        persist_prompt_path: Option<String>,
        persist_state_path: Option<String>,
    ) -> Result<Self> {
        // TODO: refactor abstract generator into trait
        let state = State::new(task)?;
        Ok(Self {
            ollama,
            model_name,
            state,
            persist_prompt_path,
            persist_state_path,
        })
    }

    fn parse_model_response(&self, model_response: &str) -> Result<Vec<Invocation>> {
        let mut invocations = vec![];

        let model_response_size = model_response.len();
        let mut current = 0;

        // TODO: initialize this just once with lazy_static
        let attr_regex = Regex::new(r#"(?m)(([^=]+)="([^"]+)")"#)?;

        // TODO: replace this with a proper xml parser
        while current < model_response_size {
            // read until < or end
            let mut ptr = &model_response[current..];
            if let Some(tag_open_idx) = ptr.find('<') {
                current += tag_open_idx;
                ptr = &ptr[tag_open_idx..];
                // read tag
                if let Some(tag_name_term_idx) = ptr.find(|c: char| c == '>' || c == ' ') {
                    current += tag_name_term_idx;
                    let tag_name = &ptr[1..tag_name_term_idx];
                    // println!("tag_name={}", tag_name);
                    if let Some(tag_close_idx) = ptr.find('>') {
                        current += tag_close_idx + tag_name.len();
                        let tag_closing = format!("</{}>", tag_name);
                        let tag_closing_idx = ptr.find(&tag_closing);
                        if let Some(tag_closing_idx) = tag_closing_idx {
                            // parse attributes if any
                            let attributes = if ptr.as_bytes()[tag_name_term_idx] == b' ' {
                                let attr_str = &ptr[tag_name_term_idx + 1..tag_close_idx];
                                let mut attrs = HashMap::new();

                                // parse as a list of key="value"
                                let iter = attr_regex.captures_iter(attr_str);
                                for caps in iter {
                                    if caps.len() == 4 {
                                        let key = caps.get(2).unwrap().as_str().trim();
                                        let value = caps.get(3).unwrap().as_str().trim();
                                        attrs.insert(key.to_string(), value.to_string());
                                    }
                                }

                                Some(attrs)
                            } else {
                                None
                            };

                            // parse payload if any
                            let after_tag_close = &ptr[tag_close_idx + 1..tag_closing_idx];
                            let payload = if !after_tag_close.is_empty() {
                                if after_tag_close.as_bytes()[0] != b'<' {
                                    Some(after_tag_close.trim().to_string())
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                            invocations.push(Invocation::new(
                                tag_name.to_string(),
                                attributes,
                                payload,
                            ));

                            continue;
                        }
                    }
                }

                // just skip ahead
                current += 1;
            } else {
                // no more tags
                break;
            }
        }

        Ok(invocations)
    }

    fn dump_state(&self) -> Result<()> {
        if let Some(state_path) = &self.persist_state_path {
            std::fs::write(state_path, self.state.to_pretty_string()?)?;
        }

        if let Some(prompt_path) = &self.persist_prompt_path {
            std::fs::write(prompt_path, &prompt_path)?;
        }

        Ok(())
    }
    pub async fn step(&mut self) -> Result<()> {
        /*
        pub struct GenerationRequest {
            ...
            TODO: images for multimodal
            pub images: Vec<Image>,
            ...
        }
        */

        // TODO: explore passing the dynamic parts of the state as user prompt instead of system prompt
        let system_prompt = self.state.to_system_prompt()?;
        let prompt = self.state.to_prompt()?;

        self.dump_state()?;

        let req = GenerationRequest::new(self.model_name.clone(), prompt)
            .system(system_prompt)
            .options(
                GenerationOptions::default()
                    .num_ctx(10000)
                    .temperature(0.9)
                    .repeat_penalty(1.3)
                    .top_k(20),
            );
        let res = self.ollama.generate(req).await?;

        // println!("response: {}\n\n", res.response);

        // parse the model response into invocations
        let invocations = self.parse_model_response(&res.response)?;
        let mut prev: Option<String> = None;

        // for each parsed invocation
        for inv in invocations {
            // avoid running the same command twince in a row
            if let Some(p) = prev.as_ref() {
                if inv.xml == *p {
                    println!(".");
                    continue;
                }
            }

            prev = Some(inv.xml.clone());

            // see if valid action and execute
            if let Err(e) = self.state.execute(inv).await {
                println!("ERROR: {}", e);
            }

            self.dump_state()?;
            if self.state.is_complete() {
                break;
            }
        }

        Ok(())
    }

    pub fn is_state_complete(&self) -> bool {
        self.state.is_complete()
    }
}