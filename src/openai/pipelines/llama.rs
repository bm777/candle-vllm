use std::{iter, path::PathBuf};

use crate::openai::{
    conversation::{
        default_conversation::{
            DefaultConversation, DefaultConversationSeparators, SeparatorStyle,
        },
        Conversation,
    },
    requests::StopTokens,
    responses::{
        APIError, ChatChoice, ChatChoiceData, ChatCompletionUsageResponse,
        StreamingChatCompletionResponse, StreamingChoice, StreamingChoiceData,
    },
    sampling_params::SamplingParams,
    streaming::SenderError,
    utils::get_created_time_secs,
    PipelineConfig, TokenizerWrapper,
};
use actix_web::web::Bytes;
use candle_core::{DType, Device, Tensor};
use candle_lora_transformers::varbuilder_utils::from_mmaped_safetensors;
use candle_sampling::logits_processor::{LogitsProcessor, SamplingMethod};
use candle_transformers::models::llama::{Cache, Llama, LlamaConfig};
use hf_hub::{api::sync::ApiBuilder, Repo, RepoType};
use tokenizers::Tokenizer;
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

use super::{ModelLoader, ModelPaths, ModulePipeline};

const EOS_TOKEN: &str = "</s>";
const NAME: &str = "llama";
const SAMPLING_SEED: u64 = 299792458;

#[derive(Debug, Clone)]
pub struct LlamaSpecificConfig {
    no_kv_cache: bool,
    repeat_last_n: usize,
    use_flash_attn: bool,
}

impl Default for LlamaSpecificConfig {
    fn default() -> Self {
        Self {
            no_kv_cache: false,
            repeat_last_n: 64,
            use_flash_attn: false,
        }
    }
}

/// top-p, multinomial, and argmax sampling are implemented. Beam search is not implemented.
pub struct LlamaPipeline {
    llama: Llama,
    args: LlamaSpecificConfig,
    cache: Cache,
    tokenizer: Tokenizer,
    conversation: DefaultConversation,
}

pub struct LlamaLoader;

pub struct LlamaModelPaths<P> {
    tokenizer_filename: P,
    config_filename: P,
    filenames: Vec<P>,
}

impl ModelPaths for LlamaModelPaths<PathBuf> {
    fn get_config_filename(&self) -> &PathBuf {
        &self.config_filename
    }
    fn get_tokenizer_filename(&self) -> &PathBuf {
        &self.tokenizer_filename
    }
    fn get_weight_filenames(&self) -> &Vec<PathBuf> {
        &self.filenames
    }
}

impl<'a> ModelLoader<'a> for LlamaLoader {
    fn download_model(
        &self,
        model_id: String,
        revision: Option<String>,
        hf_token: Option<String>,
    ) -> Result<Box<dyn ModelPaths>, APIError> {
        let api = ApiBuilder::new()
            .with_progress(true)
            .with_token(hf_token)
            .build()
            .map_err(APIError::new_from_hf_err)?;
        let revision = revision.unwrap_or("main".to_string());
        let api = api.repo(Repo::with_revision(model_id, RepoType::Model, revision));

        let tokenizer_filename = api
            .get("tokenizer.json")
            .map_err(APIError::new_from_hf_err)?;

        let config_filename = api.get("config.json").map_err(APIError::new_from_hf_err)?;

        let mut filenames = vec![];
        for rfilename in api
            .info()
            .map_err(APIError::new_from_hf_err)?
            .siblings
            .iter()
            .map(|x| x.rfilename.clone())
            .filter(|x| x.ends_with(".safetensors"))
        {
            let filename = api.get(&rfilename).map_err(APIError::new_from_hf_err)?;
            filenames.push(filename);
        }

        Ok(Box::new(LlamaModelPaths {
            tokenizer_filename,
            config_filename,
            filenames,
        }))
    }

    fn load_model(
        &self,
        paths: Box<dyn ModelPaths>,
        dtype: DType,
        device: Device,
    ) -> Result<(Box<dyn ModulePipeline<'a>>, PipelineConfig), APIError> {
        let args = LlamaSpecificConfig::default();

        let config: LlamaConfig = serde_json::from_slice(
            &std::fs::read(paths.get_config_filename()).map_err(APIError::new_from_io_err)?,
        )
        .map_err(APIError::new_from_serde_err)?;
        let config = config.into_config(args.use_flash_attn);

        println!("Loading Llama model.");

        let vb = from_mmaped_safetensors(paths.get_weight_filenames(), dtype, &device, false)
            .map_err(APIError::new_from_candle_err)?;

        let cache = Cache::new(!args.no_kv_cache, dtype, &config, &device)
            .map_err(APIError::new_from_candle_err)?;

        let llama = Llama::load(vb, &cache, &config).map_err(APIError::new_from_candle_err)?;

        let tokenizer = Tokenizer::from_file(paths.get_tokenizer_filename())
            .map_err(|x| APIError::new(x.to_string()))?;

        println!("Done loading.");

        //max is https://huggingface.co/docs/transformers/model_doc/llama2#transformers.LlamaConfig.max_position_embeddings
        let pipeline_config = PipelineConfig {
            max_model_len: 4096,
        };
        Ok((
            Box::new(LlamaPipeline {
                llama,
                args,
                cache,
                tokenizer,
                conversation: DefaultConversation::new(
                    "llama-2".to_string(),
                    "[INST] <<SYS>>\n{}\n<</SYS>>\n\n".to_string(),
                    Vec::default(),
                    0,
                    SeparatorStyle::Llama2,
                    "".to_string(),
                    Vec::default(),
                    ("[INST]".to_string(), "[/INST]".to_string()),
                    DefaultConversationSeparators {
                        sep: " ".to_string(),
                        sep2: Some(" </s></s>".to_string()),
                    },
                ),
            }),
            pipeline_config,
        ))
    }
}

impl LlamaPipeline {
    #[allow(clippy::too_many_arguments)]
    fn generate_token(
        &mut self,
        tokens: &Vec<u32>,
        logits_processor: &mut LogitsProcessor,
        tokens_generated: &mut usize,
        index_pos: &mut usize,
        context_size: usize,
        device: &Device,
        sampling: &SamplingParams,
    ) -> Result<u32, APIError> {
        let ctxt = &tokens[tokens.len().saturating_sub(context_size)..];
        let input = Tensor::new(ctxt, device)
            .map_err(APIError::new_from_candle_err)?
            .unsqueeze(0)
            .map_err(APIError::new_from_candle_err)?;
        let logits = self
            .llama
            .forward(&input, *index_pos)
            .map_err(APIError::new_from_candle_err)?;
        let logits = logits.squeeze(0).map_err(APIError::new_from_candle_err)?;
        let logits = if sampling.repetition_penalty == 1. {
            logits
        } else {
            let start_at = tokens.len().saturating_sub(self.args.repeat_last_n);
            candle_transformers::utils::apply_repeat_penalty(
                &logits,
                sampling.repetition_penalty,
                &tokens[start_at..],
            )
            .map_err(APIError::new_from_candle_err)?
        };
        *index_pos += ctxt.len();

        let next_token = logits_processor
            .sample(&logits)
            .map_err(APIError::new_from_candle_err)?;
        *tokens_generated += 1;

        Ok(next_token)
    }

    #[allow(clippy::too_many_arguments)]
    fn forward_inner(
        &mut self,
        mut tokens: Vec<u32>,
        sampling: &SamplingParams,
        device: &Device,
        eos_token_id: &Option<u32>,
        logits_processor: &mut LogitsProcessor,
        streamer: Option<Sender<Result<Bytes, SenderError>>>,
        stop_tokens: Vec<String>,
        gen_index: usize,
    ) -> Result<(Option<ChatChoice>, ChatCompletionUsageResponse), APIError> {
        match streamer {
            Some(streamer) => {
                struct StreamingGen {
                    index_pos: usize,
                    index: i32,
                    tokens_generated: usize,
                    done_reason: Option<String>,
                }

                let mut tokens_n = Vec::new();
                for _ in 0..sampling.n {
                    tokens_n.push(StreamingGen {
                        index_pos: 0,
                        index: 0,
                        tokens_generated: 0,
                        done_reason: None,
                    });
                }
                let tokens_n = &mut tokens_n[..];

                let mut total_tokens_generated = 0;

                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                while tokens_n.iter().all(|gen| gen.done_reason.is_some()) {
                    let request_id = format!("cmpl-{}", Uuid::new_v4());
                    let created = get_created_time_secs();

                    let mut choices = Vec::new();

                    for (i, gen) in iter::zip(0..sampling.n, tokens_n.iter_mut()) {
                        if gen.done_reason.is_some() {
                            choices.push(StreamingChoice {
                                delta: StreamingChoiceData {
                                    content: None,
                                    role: self.conversation.get_roles().1.clone(),
                                },
                                finish_reason: gen.done_reason.clone(),
                                index: i,
                            });
                            continue;
                        }

                        let context_size = if self.cache.use_kv_cache && gen.index > 0 {
                            1
                        } else {
                            tokens.len()
                        };

                        let next_token = self.generate_token(
                            &tokens,
                            logits_processor,
                            &mut gen.tokens_generated,
                            &mut gen.index_pos,
                            context_size,
                            device,
                            sampling,
                        )?;

                        tokens.push(next_token);
                        total_tokens_generated += 1;

                        if let Some(text) = self.tokenizer.id_to_token(next_token) {
                            let text = text.replace('▁', " ").replace("<0x0A>", "\n");

                            if stop_tokens.contains(&text) {
                                gen.done_reason = Some("stop".to_string());
                            }

                            choices.push(StreamingChoice {
                                delta: StreamingChoiceData {
                                    content: Some(text),
                                    role: "assistant".to_string(),
                                },
                                finish_reason: None,
                                index: i,
                            });
                        }

                        if gen.done_reason.is_none() {
                            if &Some(next_token) == eos_token_id
                                || sampling
                                    .stop_token_ids
                                    .contains(&(eos_token_id.unwrap() as usize))
                            {
                                gen.done_reason = Some("stop".to_string());
                            }
                            if gen.tokens_generated >= sampling.max_tokens {
                                gen.done_reason = Some("length".to_string());
                            }
                        }

                        gen.index += 1;
                    }

                    // Ignore sending errors
                    let _ = runtime.block_on(
                        streamer.send(Ok(Bytes::from(
                            serde_json::to_vec(&StreamingChatCompletionResponse {
                                choices,
                                id: request_id,
                                created,
                                model: self.name(),
                                object: "chat.completion.chunk",
                            })
                            .unwrap(),
                        ))),
                    );
                }

                Ok((
                    None,
                    ChatCompletionUsageResponse {
                        completion_tokens: total_tokens_generated,
                        prompt_tokens: tokens.len(),
                        total_tokens: total_tokens_generated + tokens.len(),
                    },
                ))
            }

            None => {
                let mut index_pos = 0;
                let mut index = 0;
                let mut result = "".to_string();
                let mut tokens_generated = 0;
                let finish_reason;

                loop {
                    let context_size = if self.cache.use_kv_cache && index > 0 {
                        1
                    } else {
                        tokens.len()
                    };

                    let next_token = self.generate_token(
                        &tokens,
                        logits_processor,
                        &mut tokens_generated,
                        &mut index_pos,
                        context_size,
                        device,
                        sampling,
                    )?;

                    tokens.push(next_token);

                    if let Some(text) = self.tokenizer.id_to_token(next_token) {
                        let text = text.replace('▁', " ").replace("<0x0A>", "\n");
                        if stop_tokens.contains(&text) {
                            finish_reason = "stop".to_string();
                            break;
                        }
                        result.push_str(&text);
                    }

                    if &Some(next_token) == eos_token_id {
                        finish_reason = "stop".to_string();
                        break;
                    }
                    if tokens_generated >= sampling.max_tokens {
                        finish_reason = "length".to_string();
                        break;
                    }

                    index += 1;
                }

                Ok((
                    Some(ChatChoice {
                        message: ChatChoiceData {
                            content: Some(result),
                            role: self.conversation.get_roles().1.clone(),
                        },
                        finish_reason: Some(finish_reason),
                        index: gen_index,
                    }),
                    ChatCompletionUsageResponse {
                        completion_tokens: tokens_generated,
                        prompt_tokens: tokens.len(),
                        total_tokens: tokens_generated + tokens.len(),
                    },
                ))
            }
        }
    }
}

impl<'s> ModulePipeline<'s> for LlamaPipeline {
    fn forward(
        &mut self,
        xs: &tokenizers::Encoding,
        sampling: SamplingParams,
        device: Device,
        streamer: Option<Sender<Result<Bytes, SenderError>>>,
    ) -> Result<(Option<Vec<ChatChoice>>, ChatCompletionUsageResponse), APIError> {
        let eos_token_id = self.tokenizer.token_to_id(EOS_TOKEN);

        let mut logits_processor = LogitsProcessor::new(
            SAMPLING_SEED,
            Some(sampling.temperature.try_into().unwrap()),
            SamplingMethod::TopP(sampling.top_p.try_into().unwrap()),
        );

        let stop_tokens = match sampling.stop.clone() {
            Some(stop) => match stop {
                StopTokens::Multi(multi) => multi,
                StopTokens::Single(single) => vec![single],
            },

            None => vec![],
        };

        let mut tokens_generated = 0;
        let mut choices = Vec::new();
        match streamer {
            Some(streamer) => {
                let tokens = xs.get_ids().to_vec();

                let (_result, _tokens_gen) = self.forward_inner(
                    tokens,
                    &sampling,
                    &device,
                    &eos_token_id,
                    &mut logits_processor,
                    Some(streamer),
                    stop_tokens,
                    usize::MAX,
                )?;
            }
            None => {
                for i in 0..sampling.n {
                    let tokens = xs.get_ids().to_vec();

                    let (result, tokens_gen) = self.forward_inner(
                        tokens,
                        &sampling,
                        &device,
                        &eos_token_id,
                        &mut logits_processor,
                        None,
                        stop_tokens.clone(),
                        i,
                    )?;
                    tokens_generated += tokens_gen.completion_tokens;
                    choices.push(result.unwrap());
                }
            }
        }

        Ok((
            Some(choices),
            ChatCompletionUsageResponse {
                completion_tokens: tokens_generated,
                prompt_tokens: xs.len(),
                total_tokens: tokens_generated + xs.len(),
            },
        ))
    }

    fn name(&self) -> String {
        NAME.to_string()
    }

    fn tokenizer(&self) -> &dyn TokenizerWrapper<'s, String> {
        &self.tokenizer
    }

    fn get_conversation(&mut self) -> &mut dyn Conversation {
        &mut self.conversation
    }
}

unsafe impl Send for LlamaPipeline {}
unsafe impl Sync for LlamaPipeline {}
