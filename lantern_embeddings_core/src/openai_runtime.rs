use itertools::Itertools;
use std::{collections::HashMap, sync::RwLock};

use crate::{
    core::LoggerFn,
    runtime::{EmbeddingResult, EmbeddingRuntime},
    HTTPRuntime,
};
use serde::{Deserialize, Serialize};
use tiktoken_rs::{cl100k_base, CoreBPE};

struct ModelInfo {
    name: String,
    tokenizer: CoreBPE,
    sequence_len: usize,
    dimensions: usize,
    var_dimension: bool,
}

#[derive(Deserialize)]
struct OpenAiEmbedding {
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    total_tokens: usize,
}

#[derive(Deserialize)]
struct OpenAiResponse {
    data: Vec<OpenAiEmbedding>,
    usage: OpenAiUsage,
}

impl ModelInfo {
    pub fn new(model_name: &str) -> Result<Self, anyhow::Error> {
        let name = model_name.split("/").last().unwrap().to_owned();
        match model_name {
            "openai/text-embedding-ada-002" => Ok(Self {
                name,
                tokenizer: cl100k_base()?,
                sequence_len: 8192,
                dimensions: 1536,
                var_dimension: false,
            }),
            "openai/text-embedding-3-small" => Ok(Self {
                name,
                tokenizer: cl100k_base()?,
                sequence_len: 8191,
                dimensions: 1536,
                var_dimension: true,
            }),
            "openai/text-embedding-3-large" => Ok(Self {
                name,
                tokenizer: cl100k_base()?,
                sequence_len: 8191,
                dimensions: 3072,
                var_dimension: true,
            }),
            _ => anyhow::bail!("Unsupported model {model_name}"),
        }
    }
}

lazy_static! {
    static ref MODEL_INFO_MAP: RwLock<HashMap<&'static str, ModelInfo>> =
        RwLock::new(HashMap::from([
            (
                "openai/text-embedding-ada-002",
                ModelInfo::new("openai/text-embedding-ada-002").unwrap()
            ),
            (
                "openai/text-embedding-3-small",
                ModelInfo::new("openai/text-embedding-3-small").unwrap()
            ),
            (
                "openai/text-embedding-3-large",
                ModelInfo::new("openai/text-embedding-3-large").unwrap()
            ),
        ]));
}

pub struct OpenAiRuntime<'a> {
    request_timeout: u64,
    base_url: String,
    headers: Vec<(String, String)>,
    dimensions: Option<usize>,
    #[allow(dead_code)]
    logger: &'a LoggerFn,
}

#[derive(Serialize, Deserialize)]
pub struct OpenAiRuntimeParams {
    pub api_token: Option<String>,
    pub dimensions: Option<usize>,
}

impl<'a> OpenAiRuntime<'a> {
    pub fn new(logger: &'a LoggerFn, params: &'a str) -> Result<Self, anyhow::Error> {
        let runtime_params: OpenAiRuntimeParams = serde_json::from_str(&params)?;

        if runtime_params.api_token.is_none() {
            anyhow::bail!("'api_token' is required for OpenAi runtime");
        }

        Ok(Self {
            base_url: "https://api.openai.com".to_owned(),
            logger,
            request_timeout: 120,
            headers: vec![
                ("Content-Type".to_owned(), "application/json".to_owned()),
                (
                    "Authorization".to_owned(),
                    format!("Bearer {}", runtime_params.api_token.unwrap()),
                ),
            ],
            dimensions: runtime_params.dimensions,
        })
    }

    fn group_vectors_by_token_count(
        &self,
        input: Vec<Vec<usize>>,
        max_token_count: usize,
    ) -> Vec<Vec<Vec<usize>>> {
        let mut result = Vec::new();
        let mut current_group = Vec::new();
        let mut current_group_token_count = 0;

        for inner_vec in input {
            let inner_vec_token_count = inner_vec.len();

            if current_group_token_count + inner_vec_token_count <= max_token_count {
                // Add the inner vector to the current group
                current_group.push(inner_vec);
                current_group_token_count += inner_vec_token_count;
            } else {
                // Start a new group
                result.push(current_group);
                current_group = vec![inner_vec];
                current_group_token_count = inner_vec_token_count;
            }
        }

        // Add the last group if it's not empty
        if !current_group.is_empty() {
            result.push(current_group);
        }

        result
    }

    fn chunk_inputs(
        &self,
        model_name: &str,
        inputs: &Vec<&str>,
    ) -> Result<Vec<String>, anyhow::Error> {
        let model_map = MODEL_INFO_MAP.read().unwrap();
        let model_info = model_map.get(model_name);

        if model_info.is_none() {
            anyhow::bail!(
                "Unsupported model {model_name}\nAvailable models: {}",
                model_map.keys().join(", ")
            );
        }

        let model_info = model_info.unwrap();
        let token_groups: Vec<Vec<usize>> = inputs
            .iter()
            .map(|input| {
                let mut tokens = model_info.tokenizer.encode_with_special_tokens(input);
                if tokens.len() > model_info.sequence_len {
                    tokens.truncate(model_info.sequence_len);
                }
                tokens
            })
            .collect();

        // Dimensions for new openai models can be specified
        let dimensions_input = if model_info.var_dimension && self.dimensions.is_some() {
            format!(r#", "dimensions": {}"#, self.dimensions.as_ref().unwrap())
        } else {
            "".to_owned()
        };

        let name = &model_info.name;
        let batch_tokens: Vec<String> = self
            .group_vectors_by_token_count(token_groups, model_info.sequence_len)
            .iter()
            .map(|token_group| {
                let json_string = serde_json::to_string(token_group).unwrap();
                format!(
                    r#"
                 {{
                   "input": {json_string},
                   "model": "{name}"
                   {dimensions_input}
                 }}
                "#
                )
            })
            .collect();

        Ok(batch_tokens)
    }

    // Static functions
    pub fn get_response(body: Vec<u8>) -> Result<EmbeddingResult, anyhow::Error> {
        let result: Result<OpenAiResponse, serde_json::Error> = serde_json::from_slice(&body);
        if let Err(e) = result {
            anyhow::bail!(
                "Error: {e}. OpenAI response: {:?}",
                serde_json::from_slice::<serde_json::Value>(&body)?
            );
        }

        let result = result.unwrap();

        Ok(EmbeddingResult {
            processed_tokens: result.usage.total_tokens,
            embeddings: result
                .data
                .iter()
                .map(|emb| emb.embedding.clone())
                .collect(),
        })
    }
}

impl<'a> EmbeddingRuntime for OpenAiRuntime<'a> {
    fn process(
        &self,
        model_name: &str,
        inputs: &Vec<&str>,
    ) -> Result<EmbeddingResult, anyhow::Error> {
        self.post_request("/v1/embeddings", model_name, inputs)
    }

    fn get_available_models(&self) -> (String, Vec<(String, bool)>) {
        let map = MODEL_INFO_MAP.read().unwrap();
        let mut res = String::new();
        let mut models = Vec::with_capacity(map.len());
        for (key, value) in &*map {
            res.push_str(&format!(
                "{} - sequence_len: {}, dimensions: {}\n",
                key, value.sequence_len, value.dimensions
            ));
            models.push((key.to_string(), false));
        }

        return (res, models);
    }
}
HTTPRuntime!(OpenAiRuntime);
