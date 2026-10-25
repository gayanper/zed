use std::sync::Arc;

use collections::HashMap;
use settings::RegisterSetting;

#[cfg(feature = "zed-cloud")]
use crate::provider::cloud::ZedDotDevSettings;
use crate::provider::{
    anthropic, anthropic::AnthropicSettings, anthropic_compatible::AnthropicCompatibleSettings,
    llama_cpp::LlamaCppSettings, lmstudio::LmStudioSettings, ollama::OllamaSettings,
    open_ai::OpenAiSettings, open_ai_compatible::OpenAiCompatibleSettings, resolve_custom_headers,
};
#[cfg(feature = "hosted-llms")]
use crate::provider::{
    bedrock, bedrock::AmazonBedrockSettings, deepseek::DeepSeekSettings, google::GoogleSettings,
    mistral, mistral::MistralSettings, open_router, open_router::OpenRouterSettings, opencode,
    opencode::OpenCodeSettings, vercel_ai_gateway::VercelAiGatewaySettings, x_ai::XAiSettings,
};

#[derive(Debug, RegisterSetting)]
pub struct AllLanguageModelSettings {
    pub anthropic: AnthropicSettings,
    pub anthropic_compatible: HashMap<Arc<str>, AnthropicCompatibleSettings>,
    #[cfg(feature = "hosted-llms")]
    pub bedrock: AmazonBedrockSettings,
    #[cfg(feature = "hosted-llms")]
    pub deepseek: DeepSeekSettings,
    #[cfg(feature = "hosted-llms")]
    pub google: GoogleSettings,
    pub llama_cpp: LlamaCppSettings,
    pub lmstudio: LmStudioSettings,
    #[cfg(feature = "hosted-llms")]
    pub mistral: MistralSettings,
    pub ollama: OllamaSettings,
    #[cfg(feature = "hosted-llms")]
    pub opencode: OpenCodeSettings,
    #[cfg(feature = "hosted-llms")]
    pub open_router: OpenRouterSettings,
    pub openai: OpenAiSettings,
    pub openai_compatible: HashMap<Arc<str>, OpenAiCompatibleSettings>,
    #[cfg(feature = "hosted-llms")]
    pub vercel_ai_gateway: VercelAiGatewaySettings,
    #[cfg(feature = "hosted-llms")]
    pub x_ai: XAiSettings,
    #[cfg(feature = "zed-cloud")]
    pub zed_dot_dev: ZedDotDevSettings,
}

fn custom_headers_from(
    provider_name: &str,
    raw: Option<HashMap<String, String>>,
    reserved: &[&str],
) -> http_client::CustomHeaders {
    raw.as_ref()
        .filter(|map| !map.is_empty())
        .map(|map| resolve_custom_headers(provider_name, map, reserved))
        .unwrap_or_default()
}

impl settings::Settings for AllLanguageModelSettings {
    const PRESERVED_KEYS: Option<&'static [&'static str]> = Some(&["version"]);

    fn from_settings(content: &settings::SettingsContent) -> Self {
        let language_models = content.language_models.clone().unwrap();
        let anthropic = language_models.anthropic.unwrap();
        let anthropic_compatible = language_models.anthropic_compatible.unwrap();
        #[cfg(feature = "hosted-llms")]
        let bedrock = language_models.bedrock.unwrap();
        #[cfg(feature = "hosted-llms")]
        let deepseek = language_models.deepseek.unwrap();
        #[cfg(feature = "hosted-llms")]
        let google = language_models.google.unwrap();
        let llama_cpp = language_models.llama_cpp.unwrap();
        let lmstudio = language_models.lmstudio.unwrap();
        #[cfg(feature = "hosted-llms")]
        let mistral = language_models.mistral.unwrap();
        let ollama = language_models.ollama.unwrap();
        #[cfg(feature = "hosted-llms")]
        let opencode = language_models.opencode.unwrap();
        #[cfg(feature = "hosted-llms")]
        let open_router = language_models.open_router.unwrap();
        let openai = language_models.openai.unwrap();
        let openai_compatible = language_models.openai_compatible.unwrap();
        #[cfg(feature = "hosted-llms")]
        let vercel_ai_gateway = language_models.vercel_ai_gateway.unwrap();
        #[cfg(feature = "hosted-llms")]
        let x_ai = language_models.x_ai.unwrap();
        #[cfg(feature = "zed-cloud")]
        let zed_dot_dev = language_models.zed_dot_dev.unwrap();
        Self {
            anthropic: AnthropicSettings {
                api_url: anthropic.api_url.unwrap(),
                available_models: anthropic.available_models.unwrap_or_default(),
                custom_headers: custom_headers_from(
                    "Anthropic",
                    anthropic.custom_headers,
                    anthropic::RESERVED_HEADER_NAMES,
                ),
            },
            anthropic_compatible: anthropic_compatible
                .into_iter()
                .map(|(key, value)| {
                    let provider_label = format!("Anthropic Compatible ({key})");
                    (
                        key,
                        AnthropicCompatibleSettings {
                            api_url: value.api_url,
                            available_models: value.available_models,
                            custom_headers: custom_headers_from(
                                &provider_label,
                                value.custom_headers,
                                anthropic::RESERVED_HEADER_NAMES,
                            ),
                        },
                    )
                })
                .collect(),
            #[cfg(feature = "hosted-llms")]
            bedrock: AmazonBedrockSettings {
                available_models: bedrock.available_models.unwrap_or_default(),
                mantle_available_models: bedrock.mantle_available_models.unwrap_or_default(),
                custom_headers: custom_headers_from(
                    "Amazon Bedrock",
                    bedrock.custom_headers,
                    bedrock::RESERVED_HEADER_NAMES,
                ),
                region: bedrock.region,
                endpoint: bedrock.endpoint_url, // todo(should be api_url)
                profile_name: bedrock.profile,
                role_arn: None, // todo(was never a setting for this...)
                authentication_method: bedrock.authentication_method.map(Into::into),
                allow_global: bedrock.allow_global,
                guardrail_identifier: bedrock.guardrail_identifier,
                guardrail_version: bedrock.guardrail_version,
            },
            #[cfg(feature = "hosted-llms")]
            deepseek: DeepSeekSettings {
                api_url: deepseek.api_url.unwrap(),
                available_models: deepseek.available_models.unwrap_or_default(),
                custom_headers: custom_headers_from("DeepSeek", deepseek.custom_headers, &[]),
            },
            #[cfg(feature = "hosted-llms")]
            google: GoogleSettings {
                api_url: google.api_url.unwrap(),
                available_models: google.available_models.unwrap_or_default(),
                custom_headers: custom_headers_from("Google AI", google.custom_headers, &[]),
            },
            llama_cpp: LlamaCppSettings {
                api_url: llama_cpp.api_url.unwrap(),
                auto_discover: llama_cpp.auto_discover.unwrap_or(true),
                available_models: llama_cpp.available_models.unwrap_or_default(),
                context_window: llama_cpp.context_window,
                custom_headers: custom_headers_from("llama.cpp", llama_cpp.custom_headers, &[]),
            },
            lmstudio: LmStudioSettings {
                api_url: lmstudio.api_url.unwrap(),
                available_models: lmstudio.available_models.unwrap_or_default(),
                custom_headers: custom_headers_from("LM Studio", lmstudio.custom_headers, &[]),
            },
            #[cfg(feature = "hosted-llms")]
            mistral: MistralSettings {
                api_url: mistral.api_url.unwrap(),
                available_models: mistral.available_models.unwrap_or_default(),
                custom_headers: custom_headers_from(
                    "Mistral",
                    mistral.custom_headers,
                    mistral::RESERVED_HEADER_NAMES,
                ),
            },
            ollama: OllamaSettings {
                api_url: ollama.api_url.unwrap(),
                auto_discover: ollama.auto_discover.unwrap_or(true),
                available_models: ollama.available_models.unwrap_or_default(),
                context_window: ollama.context_window,
                custom_headers: custom_headers_from("Ollama", ollama.custom_headers, &[]),
            },
            #[cfg(feature = "hosted-llms")]
            opencode: OpenCodeSettings {
                api_url: opencode.api_url.unwrap(),
                available_models: opencode.available_models.unwrap_or_default(),
                custom_headers: custom_headers_from(
                    "OpenCode",
                    opencode.custom_headers,
                    opencode::RESERVED_HEADER_NAMES,
                ),
                show_zen_models: opencode.show_zen_models.unwrap_or(true),
                show_go_models: opencode.show_go_models.unwrap_or(true),
            },
            #[cfg(feature = "hosted-llms")]
            open_router: OpenRouterSettings {
                api_url: open_router.api_url.unwrap(),
                available_models: open_router.available_models.unwrap_or_default(),
                custom_headers: custom_headers_from(
                    "OpenRouter",
                    open_router.custom_headers,
                    open_router::RESERVED_HEADER_NAMES,
                ),
            },
            openai: OpenAiSettings {
                api_url: openai.api_url.unwrap(),
                available_models: openai.available_models.unwrap_or_default(),
                custom_headers: custom_headers_from("OpenAI", openai.custom_headers, &[]),
            },
            openai_compatible: openai_compatible
                .into_iter()
                .map(|(key, value)| {
                    let provider_label = format!("OpenAI Compatible ({key})");
                    (
                        key,
                        OpenAiCompatibleSettings {
                            api_url: value.api_url,
                            available_models: value.available_models,
                            custom_headers: custom_headers_from(
                                &provider_label,
                                value.custom_headers,
                                &[],
                            ),
                        },
                    )
                })
                .collect(),
            #[cfg(feature = "hosted-llms")]
            vercel_ai_gateway: VercelAiGatewaySettings {
                api_url: vercel_ai_gateway.api_url.unwrap(),
                available_models: vercel_ai_gateway.available_models.unwrap_or_default(),
                custom_headers: custom_headers_from(
                    "Vercel AI Gateway",
                    vercel_ai_gateway.custom_headers,
                    &[],
                ),
            },
            #[cfg(feature = "hosted-llms")]
            x_ai: XAiSettings {
                api_url: x_ai.api_url.unwrap(),
                available_models: x_ai.available_models.unwrap_or_default(),
                custom_headers: custom_headers_from("xAI", x_ai.custom_headers, &[]),
            },
            #[cfg(feature = "zed-cloud")]
            zed_dot_dev: ZedDotDevSettings {
                available_models: zed_dot_dev.available_models.unwrap_or_default(),
            },
        }
    }
}
