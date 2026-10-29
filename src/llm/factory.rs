//! Provider settings, resolved from explicit values or the environment.
//!
//! One [`ProviderSettings`] is the whole configuration a single provider needs:
//! where it lives, which model to ask, how to authenticate and what thinking
//! effort to request. Every environment read is confined to [`mid_from_env`]
//! and [`giga_from_env`], so the resolution rule itself is pure and unit-tested
//! with no key and no environment mutation. The two roles are canonical: mid
//! reads only `LLM_MID_*`, giga reads only `LLM_GIGA_*`, and no old provider
//! variable is ever consulted.

use crate::api::Auth;
use crate::llm::LlmProvider;
use crate::llm::Mode;
use crate::llm::anthropic::AnthropicProvider;
use crate::llm::chat_completions::ChatProvider;
use crate::llm::responses_api::ResponsesProvider;
use std::env;

/// The mid provider's unchanged local default endpoint.
const DEFAULT_PROVIDER_URL: &str = "http://192.168.56.1:1234/v1/chat/completions";
/// The mid provider's unchanged local default model.
const DEFAULT_MODEL: &str = "local-model";

/// The documented Anthropic Messages endpoint a blank or absent
/// `LLM_SMOLL_URL` resolves to.
pub const DEFAULT_SMOLL_URL: &str = "https://api.anthropic.com/v1/messages";

/// The resolved settings for one provider. A provider is built by cloning one
/// settings value per turn, so the value is `Clone` and no provider holds a
/// borrowed configuration.
#[derive(Clone)]
pub struct ProviderSettings {
    pub url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub auth_disabled: bool,
    pub reasoning_effort: Option<String>,
    /// Optional provider-specific input price in USD per million tokens. It is
    /// local metadata only: missing means unpriced, and it never changes a
    /// credential, endpoint, model or request body.
    pub input_price_per_million_usd: Option<f64>,
}

impl ProviderSettings {
    /// The typed authentication this provider's requests carry: Bearer with the
    /// configured key when auth is enabled and a key exists, and no
    /// authentication otherwise. Both providers select it here, so neither has
    /// to know how a scheme becomes a header.
    pub fn auth(&self) -> Auth {
        match (&self.api_key, self.auth_disabled) {
            (Some(key), false) => Auth::Bearer(key.clone()),
            _ => Auth::None,
        }
    }
}

/// The one message for a blank Smoll key: the endpoint has no keyless mode, so
/// a blank key cannot become a provider at all.
const MISSING_SMOLL_KEY_MESSAGE: &str = concat!(
    "The smoll provider requires a non-blank API key: set LLM_SMOLL_API_KEY ",
    "for the Anthropic Messages endpoint."
);

/// The resolved settings for the native Anthropic Messages provider.
///
/// Smoll has its own coordinates and its own authentication scheme, so its
/// settings are deliberately separate from [`ProviderSettings`]: no default,
/// key or effort from the mid or giga blocks is shared with it. The API key is
/// required rather than optional -- this endpoint has no keyless mode and no
/// auth-disable switch -- so the field is a plain string that is never blank.
#[derive(Clone)]
pub struct SmollSettings {
    pub url: String,
    pub model: String,
    pub api_key: String,
    /// Optional input price in USD per million tokens, carried beside the
    /// provider's own settings and never onto the wire.
    pub input_price_per_million_usd: Option<f64>,
}

impl SmollSettings {
    /// Build the settings for one Anthropic Messages endpoint from explicit
    /// values. The key is required and must carry something other than
    /// whitespace: a blank key is refused here, by the one message every caller
    /// reports, instead of reaching the wire as an unauthenticated request.
    pub fn new(
        url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<SmollSettings, String> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(MISSING_SMOLL_KEY_MESSAGE.to_string());
        }
        Ok(SmollSettings {
            url: url.into(),
            model: model.into(),
            api_key,
            input_price_per_million_usd: None,
        })
    }

    /// Anthropic's native `x-api-key` scheme, carrying the configured key.
    /// There is no unauthenticated Smoll request: every valid configuration
    /// holds a non-blank key.
    pub fn auth(&self) -> Auth {
        Auth::Anthropic(self.api_key.clone())
    }
}

/// `Some(value)` kept only when it carries something other than whitespace.
fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// Parse one optional provider input rate. An absent or blank value is missing;
/// a nonblank value must parse as a finite, non-negative number of USD per
/// million input tokens, and the diagnostic always names the exact variable.
fn input_price(variable: &str, raw: Option<String>) -> Result<Option<f64>, String> {
    let Some(raw) = non_blank(raw) else {
        return Ok(None);
    };
    match raw.trim().parse::<f64>() {
        Ok(rate) if rate.is_finite() && rate >= 0.0 => Ok(Some(rate)),
        _ => Err(format!(
            "{variable} must be a finite, non-negative number of USD per million input tokens."
        )),
    }
}

/// Resolve one provider's settings from explicit values.
///
/// `None` unless both the URL and the model carry a non-blank value: a
/// half-written provider block is a configuration mistake, not a provider with
/// defaults. `auth` of `none` -- trimmed, case-insensitively -- disables auth,
/// and a blank `reasoning_effort` means the provider's own default applies.
fn settings(
    url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    auth: Option<String>,
    reasoning_effort: Option<String>,
) -> Option<ProviderSettings> {
    let url = non_blank(url)?;
    let model = non_blank(model)?;
    let auth_disabled = auth
        .map(|value| value.trim().eq_ignore_ascii_case("none"))
        .unwrap_or(false);
    Some(ProviderSettings {
        url,
        model,
        api_key: non_blank(api_key),
        auth_disabled,
        reasoning_effort: non_blank(reasoning_effort),
        input_price_per_million_usd: None,
    })
}

/// Resolve the mid provider's settings from already-read values.
///
/// The URL and model keep the unchanged local defaults when their own canonical
/// value is absent or blank, and each overrides its default independently. A
/// blank key or reasoning effort means unset, and `auth` of `none` -- trimmed,
/// case-insensitively -- disables auth.
fn mid_from_values(
    url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    auth: Option<String>,
    reasoning_effort: Option<String>,
) -> ProviderSettings {
    let url = non_blank(url).unwrap_or_else(|| DEFAULT_PROVIDER_URL.to_string());
    let model = non_blank(model).unwrap_or_else(|| DEFAULT_MODEL.to_string());
    settings(Some(url), Some(model), api_key, auth, reasoning_effort)
        .expect("the default mid URL and model are never blank")
}

/// The environment a resolver reads, as one lookup by canonical variable name.
/// Production passes [`env_value`], the process environment; tests pass an
/// isolated table, so the canonical binding is asserted without reading or
/// mutating the process environment.
pub type Environment<'a> = &'a dyn Fn(&str) -> Option<String>;

/// One variable from the process environment: the module's only environment
/// read, so every resolver has a deterministic seam.
pub fn env_value(name: &str) -> Option<String> {
    env::var(name).ok()
}

/// The mid provider's settings from the process environment.
pub fn mid_from_env() -> Result<ProviderSettings, String> {
    mid_from_environment(&env_value)
}

/// The mid provider's settings from `environment`. The key, auth and effort
/// come only from `LLM_MID_API_KEY`, `LLM_MID_AUTH` and
/// `LLM_MID_REASONING_EFFORT`; `LLM_MID_URL` and `LLM_MID_MODEL` override
/// their own defaults independently. The optional price comes only from
/// `LLM_MID_INPUT_PRICE_PER_MILLION_USD`; no old provider variable is
/// consulted.
pub fn mid_from_environment(environment: Environment<'_>) -> Result<ProviderSettings, String> {
    let mut resolved = mid_from_values(
        environment("LLM_MID_URL"),
        environment("LLM_MID_MODEL"),
        environment("LLM_MID_API_KEY"),
        environment("LLM_MID_AUTH"),
        environment("LLM_MID_REASONING_EFFORT"),
    );
    resolved.input_price_per_million_usd = input_price(
        "LLM_MID_INPUT_PRICE_PER_MILLION_USD",
        environment("LLM_MID_INPUT_PRICE_PER_MILLION_USD"),
    )?;
    Ok(resolved)
}

/// Resolve the giga provider's settings from already-read values. The block has
/// no defaults: both coordinates must carry a non-blank value, and a
/// half-written block resolves to `None`.
fn giga_from_values(
    url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    auth: Option<String>,
    reasoning_effort: Option<String>,
) -> Option<ProviderSettings> {
    settings(url, model, api_key, auth, reasoning_effort)
}

/// The giga provider's settings from the process environment.
pub fn giga_from_env() -> Result<Option<ProviderSettings>, String> {
    giga_from_environment(&env_value)
}

/// The giga provider's settings from `environment`. The key, auth and effort
/// come only from `LLM_GIGA_API_KEY`, `LLM_GIGA_AUTH` and
/// `LLM_GIGA_REASONING_EFFORT`; both `LLM_GIGA_URL` and `LLM_GIGA_MODEL` are
/// required. The optional price comes only from
/// `LLM_GIGA_INPUT_PRICE_PER_MILLION_USD`; no old provider variable is
/// consulted.
pub fn giga_from_environment(
    environment: Environment<'_>,
) -> Result<Option<ProviderSettings>, String> {
    let input_price = input_price(
        "LLM_GIGA_INPUT_PRICE_PER_MILLION_USD",
        environment("LLM_GIGA_INPUT_PRICE_PER_MILLION_USD"),
    )?;
    let Some(mut resolved) = giga_from_values(
        environment("LLM_GIGA_URL"),
        environment("LLM_GIGA_MODEL"),
        environment("LLM_GIGA_API_KEY"),
        environment("LLM_GIGA_AUTH"),
        environment("LLM_GIGA_REASONING_EFFORT"),
    ) else {
        return Ok(None);
    };
    resolved.input_price_per_million_usd = input_price;
    Ok(Some(resolved))
}

/// The Smoll coordinate that is missing when either required value is absent
/// or blank: the model first, then the key. `None` means both carry a usable
/// value. Pure, over already-read strings.
pub fn missing_smoll_variable(model: Option<&str>, api_key: Option<&str>) -> Option<&'static str> {
    let model = model.filter(|value| !value.trim().is_empty());
    let api_key = api_key.filter(|value| !value.trim().is_empty());
    match (model, api_key) {
        (None, _) => Some("LLM_SMOLL_MODEL"),
        (Some(_), None) => Some("LLM_SMOLL_API_KEY"),
        _ => None,
    }
}

/// The Smoll provider's settings from the process environment.
pub fn smoll_from_env() -> Result<SmollSettings, String> {
    smoll_from_environment(&env_value)
}

/// The Smoll provider's settings from `environment`. Smoll has no fallback:
/// both `LLM_SMOLL_MODEL` and `LLM_SMOLL_API_KEY` must carry a nonblank
/// value, and the diagnostic names whichever is missing with the configuration
/// guidance. `LLM_SMOLL_URL` is optional: an absent or blank value uses
/// [`DEFAULT_SMOLL_URL`], and a nonblank override is carried through as
/// supplied. No Mid or Giga variable is ever consulted.
pub fn smoll_from_environment(environment: Environment<'_>) -> Result<SmollSettings, String> {
    let model = environment("LLM_SMOLL_MODEL");
    let api_key = environment("LLM_SMOLL_API_KEY");
    if let Some(missing) = missing_smoll_variable(model.as_deref(), api_key.as_deref()) {
        return Err(format!(
            "The smoll provider is required for the summary phase: {missing} is not set. \
             Set LLM_SMOLL_MODEL and LLM_SMOLL_API_KEY (and optionally LLM_SMOLL_URL) to \
             configure the Anthropic Messages endpoint the summary pass uses."
        ));
    }

    let input_price = input_price(
        "LLM_SMOLL_INPUT_PRICE_PER_MILLION_USD",
        environment("LLM_SMOLL_INPUT_PRICE_PER_MILLION_USD"),
    )?;

    Ok(SmollSettings {
        url: non_blank(environment("LLM_SMOLL_URL"))
            .unwrap_or_else(|| DEFAULT_SMOLL_URL.to_string()),
        model: non_blank(model).expect("the model was checked as present"),
        api_key: non_blank(api_key).expect("the key was checked as present"),
        input_price_per_million_usd: input_price,
    })
}

/// The mode named by `AGENT_MODE`, before the environment is read: unset, empty
/// or whitespace-only is [`Mode::Mid`], a known canonical name is that mode,
/// and anything else is the one error message the fatal path prints. Pure, so
/// the rule is unit-tested without touching the environment.
pub fn mode_from_raw(raw: Option<&str>) -> Result<Mode, String> {
    match raw.map(str::trim).filter(|name| !name.is_empty()) {
        None => Ok(Mode::Mid),
        Some(name) => Mode::from_value(name).ok_or_else(|| {
            format!("AGENT_MODE=\"{name}\" is not recognised; use mid, giga or hybrid.")
        }),
    }
}

/// The mode named by the process environment's `AGENT_MODE`.
pub fn mode_from_env() -> Result<Mode, String> {
    mode_from_environment(&env_value)
}

/// The mode named by `environment`'s `AGENT_MODE`, resolving the value once
/// and handing it to [`mode_from_raw`].
pub fn mode_from_environment(environment: Environment<'_>) -> Result<Mode, String> {
    mode_from_raw(environment("AGENT_MODE").as_deref())
}

/// The giga coordinate that is missing when exactly one of `url` and `model`
/// carries a non-blank value, and `None` when the block is complete or wholly
/// absent. A present-but-blank value counts as absent, like every other blank in
/// this module. Pure, over already-read strings.
pub fn missing_giga_variable(url: Option<&str>, model: Option<&str>) -> Option<&'static str> {
    let url = url.filter(|value| !value.trim().is_empty());
    let model = model.filter(|value| !value.trim().is_empty());
    match (url, model) {
        (Some(_), None) => Some("LLM_GIGA_MODEL"),
        (None, Some(_)) => Some("LLM_GIGA_URL"),
        _ => None,
    }
}

/// The fatal message when `mode` needs the giga provider but `has_giga` is
/// false, `None` otherwise. Pure, so the requirement is unit-tested without
/// building a configuration.
pub fn giga_requirement(mode: Mode, has_giga: bool) -> Option<String> {
    match (mode.needs_giga(), has_giga) {
        (true, false) => Some(format!(
            "AGENT_MODE={} needs the giga provider: set LLM_GIGA_URL and LLM_GIGA_MODEL.",
            mode.label()
        )),
        _ => None,
    }
}

/// Build the chat-completions provider for `settings`.
pub fn chat_provider(settings: ProviderSettings) -> Box<dyn LlmProvider> {
    Box::new(ChatProvider::new(settings))
}

/// Build the responses provider for `settings`.
pub fn responses_provider(settings: ProviderSettings) -> Box<dyn LlmProvider> {
    Box::new(ResponsesProvider::new(settings))
}

/// Build the native Anthropic Messages provider for `settings`.
pub fn anthropic_provider(settings: SmollSettings) -> Box<dyn LlmProvider> {
    Box::new(AnthropicProvider::new(settings))
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "http://example.test/v1/chat/completions";
    const MODEL: &str = "test-model";
    const SMOLL_URL: &str = "https://smoll.test/v1/messages";
    const SMOLL_MODEL: &str = "smoll-model";

    /// An isolated environment: a table consulted by canonical variable name.
    fn environment<'a>(
        values: &'a [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    /// Resolve an isolated environment that must fail, or panic with the
    /// message that says it unexpectedly succeeded.
    fn smoll_error(values: &[(&'static str, &'static str)]) -> String {
        match smoll_from_environment(&environment(values)) {
            Ok(_) => panic!("the environment must not resolve: {values:?}"),
            Err(message) => message,
        }
    }

    fn url() -> Option<String> {
        Some(URL.to_string())
    }

    fn model() -> Option<String> {
        Some(MODEL.to_string())
    }

    fn secret() -> Option<String> {
        Some("secret".to_string())
    }

    /// A complete provider environment with one optional input-rate override
    /// per provider.
    fn priced_environment(
        mid_price: Option<&'static str>,
        giga_price: Option<&'static str>,
        smoll_price: Option<&'static str>,
    ) -> Vec<(&'static str, &'static str)> {
        let mut values = vec![
            ("LLM_MID_URL", URL),
            ("LLM_MID_MODEL", MODEL),
            ("LLM_GIGA_URL", "http://giga.test/v1/responses"),
            ("LLM_GIGA_MODEL", "giga-model"),
            ("LLM_SMOLL_MODEL", SMOLL_MODEL),
            ("LLM_SMOLL_API_KEY", "smoll-key"),
        ];
        for (name, price) in [
            ("LLM_MID_INPUT_PRICE_PER_MILLION_USD", mid_price),
            ("LLM_GIGA_INPUT_PRICE_PER_MILLION_USD", giga_price),
            ("LLM_SMOLL_INPUT_PRICE_PER_MILLION_USD", smoll_price),
        ] {
            if let Some(price) = price {
                values.push((name, price));
            }
        }
        values
    }

    /// Resolve the three optional rates from one complete environment.
    fn resolved_rates(
        values: &[(&'static str, &'static str)],
    ) -> (Option<f64>, Option<f64>, Option<f64>) {
        let mid = mid_from_environment(&environment(values)).expect("the mid block resolves");
        let giga = giga_from_environment(&environment(values))
            .expect("the giga block resolves")
            .expect("the giga block is configured");
        let smoll = smoll_from_environment(&environment(values)).expect("the smoll block resolves");
        (
            mid.input_price_per_million_usd,
            giga.input_price_per_million_usd,
            smoll.input_price_per_million_usd,
        )
    }

    /// Every canonical mode name is recognised after trimming and lowercasing.
    #[test]
    fn when_parsing_a_mode_name_given_every_canonical_spelling_then_recognises_it_after_trimming_and_case_folding()
     {
        assert_eq!(Mode::from_value("mid"), Some(Mode::Mid));
        assert_eq!(Mode::from_value(" giga "), Some(Mode::Giga));
        assert_eq!(Mode::from_value("HYBRID"), Some(Mode::Hybrid));
        assert_eq!(Mode::from_value("hybrid "), Some(Mode::Hybrid));
        assert_eq!(Mode::from_value("  MiD"), Some(Mode::Mid));
    }

    /// The old role names are not environment modes any more.
    #[test]
    fn when_parsing_a_mode_name_given_legacy_junk_or_blank_input_then_rejects_it() {
        assert_eq!(Mode::from_value("worker"), None);
        assert_eq!(Mode::from_value(" architect "), None);
        assert_eq!(Mode::from_value("junk"), None);
        assert_eq!(Mode::from_value(""), None);
        assert_eq!(Mode::from_value("   "), None);
    }

    /// The labels are the canonical lowercase names.
    #[test]
    fn when_reporting_a_mode_given_each_canonical_mode_then_uses_its_lowercase_name() {
        assert_eq!(Mode::Mid.label(), "mid");
        assert_eq!(Mode::Giga.label(), "giga");
        assert_eq!(Mode::Hybrid.label(), "hybrid");
    }

    /// A complete configuration resolves with every field carried through.
    #[test]
    fn when_resolving_a_provider_configuration_given_every_value_then_carries_each_field_through() {
        let resolved = settings(url(), model(), secret(), None, Some("high".to_string()))
            .expect("a complete configuration resolves");

        assert_eq!(resolved.url, URL);
        assert_eq!(resolved.model, MODEL);
        assert_eq!(resolved.api_key.as_deref(), Some("secret"));
        assert!(!resolved.auth_disabled);
        assert_eq!(resolved.reasoning_effort.as_deref(), Some("high"));
    }

    /// A missing or blank URL or model makes the whole configuration unusable.
    #[test]
    fn when_resolving_a_provider_configuration_given_a_missing_or_blank_url_or_model_then_refuses_the_block()
     {
        assert!(settings(None, model(), None, None, None).is_none());
        assert!(settings(url(), None, None, None, None).is_none());
        assert!(settings(Some("   ".to_string()), model(), None, None, None).is_none());
        assert!(settings(url(), Some("  ".to_string()), None, None, None).is_none());
        assert!(settings(None, None, None, None, None).is_none());
    }

    /// A blank effort is unset, not an empty string sent to the provider.
    #[test]
    fn when_resolving_a_provider_configuration_given_a_blank_effort_then_leaves_it_unset() {
        let resolved = settings(url(), model(), secret(), None, Some("   ".to_string()))
            .expect("url and model resolve");

        assert_eq!(resolved.reasoning_effort, None);
    }

    /// `auth = Some(" none ")` disables auth after trimming and folding case.
    #[test]
    fn when_resolving_a_provider_configuration_given_whitespaced_none_auth_then_disables_authentication()
     {
        let resolved = settings(
            url(),
            model(),
            secret(),
            Some(" none ".to_string()),
            Some("high".to_string()),
        )
        .expect("url and model resolve");

        assert!(resolved.auth_disabled);
    }

    /// A blank key is no key at all.
    #[test]
    fn when_resolving_a_provider_configuration_given_a_blank_key_then_leaves_it_unset() {
        let resolved = settings(url(), model(), Some("  ".to_string()), None, None)
            .expect("url and model resolve");

        assert_eq!(resolved.api_key, None);
    }

    /// With no override, mid keeps the unchanged local defaults.
    #[test]
    fn when_configuring_given_mid_without_overrides_then_uses_local_defaults() {
        let resolved = mid_from_values(None, None, None, None, None);

        assert_eq!(resolved.url, DEFAULT_PROVIDER_URL);
        assert_eq!(resolved.model, DEFAULT_MODEL);
        assert_eq!(resolved.api_key, None);
        assert!(!resolved.auth_disabled);
        assert_eq!(resolved.reasoning_effort, None);
    }

    /// Blank canonical mid values behave as unset, so both defaults survive.
    #[test]
    fn when_configuring_given_blank_mid_values_then_uses_local_defaults() {
        let resolved = mid_from_values(
            Some("   ".to_string()),
            Some("\t".to_string()),
            Some("  ".to_string()),
            Some(" ".to_string()),
            Some(" ".to_string()),
        );

        assert_eq!(resolved.url, DEFAULT_PROVIDER_URL);
        assert_eq!(resolved.model, DEFAULT_MODEL);
        assert_eq!(resolved.api_key, None);
        assert!(!resolved.auth_disabled);
        assert_eq!(resolved.reasoning_effort, None);
    }

    /// A nonblank canonical mid URL or model overrides only its own default.
    #[test]
    fn when_configuring_given_one_mid_override_then_keeps_the_other_default() {
        let url_only = mid_from_values(url(), None, None, None, None);
        assert_eq!(url_only.url, URL);
        assert_eq!(url_only.model, DEFAULT_MODEL);

        let model_only = mid_from_values(None, model(), None, None, None);
        assert_eq!(model_only.url, DEFAULT_PROVIDER_URL);
        assert_eq!(model_only.model, MODEL);
    }

    /// Every canonical value is carried through, including the maximum effort.
    #[test]
    fn when_configuring_given_all_canonical_values_then_preserves_max_effort() {
        let resolved = mid_from_values(
            url(),
            model(),
            secret(),
            Some("NONE".to_string()),
            Some("max".to_string()),
        );

        assert_eq!(resolved.url, URL);
        assert_eq!(resolved.model, MODEL);
        assert_eq!(resolved.api_key.as_deref(), Some("secret"));
        assert!(resolved.auth_disabled);
        assert_eq!(resolved.reasoning_effort.as_deref(), Some("max"));
    }

    /// A complete giga block resolves; a half-written or absent one does not.
    #[test]
    fn when_configuring_given_giga_coordinates_then_requires_both() {
        let resolved = giga_from_values(
            url(),
            model(),
            secret(),
            Some("none".to_string()),
            Some("max".to_string()),
        )
        .expect("both coordinates resolve");

        assert_eq!(resolved.url, URL);
        assert_eq!(resolved.model, MODEL);
        assert_eq!(resolved.api_key.as_deref(), Some("secret"));
        assert!(resolved.auth_disabled);
        assert_eq!(resolved.reasoning_effort.as_deref(), Some("max"));

        assert!(giga_from_values(None, None, None, None, None).is_none());
        assert!(giga_from_values(url(), None, None, None, None).is_none());
        assert!(giga_from_values(None, model(), None, None, None).is_none());
        assert!(
            giga_from_values(
                Some("  ".to_string()),
                Some("  ".to_string()),
                None,
                None,
                None
            )
            .is_none()
        );
    }

    /// A complete or wholly absent giga block is not half-written.
    #[test]
    fn when_checking_giga_coordinates_given_a_complete_or_absent_block_then_reports_no_missing_variable()
     {
        assert_eq!(missing_giga_variable(None, None), None);
        assert_eq!(
            missing_giga_variable(Some("http://giga.test/v1/responses"), Some("m")),
            None
        );
    }

    /// Exactly one present coordinate names the other canonical variable.
    #[test]
    fn when_checking_giga_coordinates_given_exactly_one_coordinate_then_names_the_other_variable() {
        assert_eq!(
            missing_giga_variable(Some("http://giga.test/v1/responses"), None),
            Some("LLM_GIGA_MODEL")
        );
        assert_eq!(
            missing_giga_variable(None, Some("giga-model")),
            Some("LLM_GIGA_URL")
        );
    }

    /// A present-but-blank coordinate counts as absent, like every other blank.
    #[test]
    fn when_checking_giga_coordinates_given_a_blank_coordinate_then_treats_it_as_absent() {
        assert_eq!(
            missing_giga_variable(Some("   "), Some("giga-model")),
            Some("LLM_GIGA_URL")
        );
        assert_eq!(
            missing_giga_variable(Some("http://giga.test/v1/responses"), Some("  ")),
            Some("LLM_GIGA_MODEL")
        );
        assert_eq!(missing_giga_variable(Some("  "), Some("   ")), None);
    }

    /// Only giga and hybrid modes require the giga provider.
    #[test]
    fn when_requiring_giga_given_mid_or_a_satisfied_block_then_reports_no_requirement() {
        assert_eq!(giga_requirement(Mode::Mid, false), None);
        assert_eq!(giga_requirement(Mode::Mid, true), None);
        assert_eq!(giga_requirement(Mode::Giga, true), None);
        assert_eq!(giga_requirement(Mode::Hybrid, true), None);
    }

    /// The missing-giga message names the mode and both canonical variables.
    #[test]
    fn when_requiring_giga_given_giga_or_hybrid_without_the_block_then_names_the_mode_and_both_variables()
     {
        assert_eq!(
            giga_requirement(Mode::Giga, false).as_deref(),
            Some("AGENT_MODE=giga needs the giga provider: set LLM_GIGA_URL and LLM_GIGA_MODEL.")
        );
        assert_eq!(
            giga_requirement(Mode::Hybrid, false).as_deref(),
            Some("AGENT_MODE=hybrid needs the giga provider: set LLM_GIGA_URL and LLM_GIGA_MODEL.")
        );
    }

    /// An unset, empty or whitespace-only `AGENT_MODE` is the mid mode, and
    /// every canonical name is accepted after trimming and folding case.
    #[test]
    fn when_reading_the_configured_mode_given_unset_blank_or_canonical_values_then_resolves_each() {
        assert_eq!(mode_from_raw(None), Ok(Mode::Mid));
        assert_eq!(mode_from_raw(Some("")), Ok(Mode::Mid));
        assert_eq!(mode_from_raw(Some("   ")), Ok(Mode::Mid));
        assert_eq!(mode_from_raw(Some("mid")), Ok(Mode::Mid));
        assert_eq!(mode_from_raw(Some(" Giga ")), Ok(Mode::Giga));
        assert_eq!(mode_from_raw(Some("HYBRID")), Ok(Mode::Hybrid));
    }

    /// The old role names and any other nonblank value are one exact message
    /// carrying the trimmed raw value and the canonical guidance.
    #[test]
    fn when_starting_given_unknown_nonblank_mode_then_reports_trimmed_value() {
        for (raw, trimmed) in [
            ("  deepseek  ", "deepseek"),
            ("worker", "worker"),
            (" architect ", "architect"),
        ] {
            let error = mode_from_raw(Some(raw)).unwrap_err();
            assert_eq!(
                error,
                format!("AGENT_MODE=\"{trimmed}\" is not recognised; use mid, giga or hybrid.")
            );
        }
    }

    /// The factory builds a chat-completions provider from the settings.
    #[test]
    fn when_building_a_provider_given_mid_configuration_then_reports_the_chat_completions_name() {
        let resolved = mid_from_values(url(), model(), secret(), None, Some("high".to_string()));
        assert_eq!(chat_provider(resolved).name(), "chat-completions");
    }

    /// The factory builds a responses provider from the settings.
    #[test]
    fn when_building_a_provider_given_giga_configuration_then_reports_the_responses_name() {
        let resolved = giga_from_values(url(), model(), secret(), None, Some("high".to_string()))
            .expect("both coordinates resolve");
        assert_eq!(responses_provider(resolved).name(), "responses");
    }

    /// An enabled key selects the Bearer scheme and carries that exact key, so
    /// the existing providers keep the authentication they send today.
    #[test]
    fn when_resolving_authentication_given_an_enabled_key_then_selects_bearer_with_that_key() {
        let resolved =
            settings(url(), model(), secret(), None, None).expect("url and model resolve");

        assert_eq!(resolved.auth(), Auth::Bearer("secret".to_string()));
    }

    /// The auth switch wins over a configured key: no authentication is sent.
    #[test]
    fn when_resolving_authentication_given_a_disabled_switch_then_selects_no_authentication() {
        let resolved = settings(url(), model(), secret(), Some("none".to_string()), None)
            .expect("url and model resolve");

        assert!(resolved.auth_disabled);
        assert_eq!(resolved.auth(), Auth::None);
    }

    /// A provider configured without a key selects no authentication, exactly as
    /// before: an absent key never becomes a Bearer header.
    #[test]
    fn when_resolving_authentication_given_no_key_then_selects_no_authentication() {
        let resolved = mid_from_values(url(), model(), None, None, None);

        assert_eq!(resolved.auth(), Auth::None);
    }

    /// The factory builds a native Messages provider from the separate Smoll
    /// settings.
    #[test]
    fn when_building_a_provider_given_smoll_settings_then_reports_the_anthropic_messages_name() {
        let settings =
            SmollSettings::new("http://smoll.test/v1/messages", "smoll-model", "smoll-key")
                .expect("a non-blank key configures the provider");

        assert_eq!(anthropic_provider(settings).name(), "anthropic-messages");
    }

    /// Smoll authentication is Anthropic's own scheme, not Bearer.
    #[test]
    fn when_resolving_smoll_authentication_given_an_enabled_key_then_selects_anthropic_authentication()
     {
        let settings =
            SmollSettings::new("http://smoll.test/v1/messages", "smoll-model", "smoll-key")
                .expect("a non-blank key configures the provider");

        assert_eq!(settings.auth(), Auth::Anthropic("smoll-key".to_string()));
    }

    /// Contract: the key is required, so a blank explicit key yields no settings
    /// at all -- there is no unauthenticated Smoll configuration to select.
    #[test]
    fn when_configuring_given_a_blank_explicit_smoll_key_then_refuses_the_settings() {
        for blank in ["", "   ", "\t \n"] {
            match SmollSettings::new(SMOLL_URL, SMOLL_MODEL, blank) {
                Ok(_) => panic!("a blank key must not configure the provider: {blank:?}"),
                Err(message) => {
                    assert!(
                        message.contains("LLM_SMOLL_API_KEY"),
                        "{blank:?}: {message}"
                    );
                }
            }
        }
    }

    /// A blank key resolves to no key, so it never reaches the wire as a Bearer
    /// header.
    #[test]
    fn when_resolving_authentication_given_a_blank_key_then_selects_no_authentication() {
        let resolved = mid_from_values(url(), model(), Some("   ".to_string()), None, None);

        assert_eq!(resolved.auth(), Auth::None);
    }

    /// Edge-case table: the required Smoll model and key are each refused when
    /// absent, empty or whitespace-only, and the diagnostic names the exact
    /// canonical variable plus the configuration guidance.
    #[test]
    fn when_resolving_given_missing_empty_or_whitespace_smoll_coordinates_then_names_the_missing_variable()
     {
        for missing in ["LLM_SMOLL_MODEL", "LLM_SMOLL_API_KEY"] {
            for blank in [
                None,
                Some(""),
                Some("   "),
                Some(
                    "	 
",
                ),
            ] {
                let (model, api_key) = if missing == "LLM_SMOLL_MODEL" {
                    (blank, Some("smoll-key"))
                } else {
                    (Some(SMOLL_MODEL), blank)
                };
                let mut values = Vec::new();
                if let Some(value) = model {
                    values.push(("LLM_SMOLL_MODEL", value));
                }
                if let Some(value) = api_key {
                    values.push(("LLM_SMOLL_API_KEY", value));
                }

                let error = smoll_error(&values);
                assert!(error.contains(missing), "{missing}: {error}");
                assert!(error.contains("LLM_SMOLL_MODEL"), "{missing}: {error}");
                assert!(error.contains("LLM_SMOLL_API_KEY"), "{missing}: {error}");
                assert!(error.contains("Set"), "{missing}: {error}");
            }
        }
    }

    /// Empty/zero/nil boundary: an absent, empty or whitespace-only URL is not a
    /// provider endpoint at all; the documented Messages endpoint is used.
    #[test]
    fn when_resolving_given_unset_or_blank_smoll_url_then_uses_the_documented_default() {
        for url in [
            None,
            Some(""),
            Some("   "),
            Some(
                "	 
",
            ),
        ] {
            let mut values = vec![
                ("LLM_SMOLL_MODEL", SMOLL_MODEL),
                ("LLM_SMOLL_API_KEY", "smoll-key"),
            ];
            if let Some(value) = url {
                values.push(("LLM_SMOLL_URL", value));
            }

            let resolved = smoll_from_environment(&environment(&values))
                .expect("the required coordinates resolve");
            assert_eq!(resolved.url, DEFAULT_SMOLL_URL, "url {url:?}");
            assert_eq!(
                resolved.url, "https://api.anthropic.com/v1/messages",
                "the documented Messages endpoint"
            );
        }
    }

    /// Upper boundary: nonblank values are carried through as supplied, above
    /// all a nonblank URL override, and the key selects Anthropic auth.
    #[test]
    fn when_resolving_given_nonblank_smoll_values_then_carries_model_key_and_url_through() {
        let values = [
            ("LLM_SMOLL_URL", SMOLL_URL),
            ("LLM_SMOLL_MODEL", SMOLL_MODEL),
            ("LLM_SMOLL_API_KEY", "smoll-key"),
        ];

        let resolved = smoll_from_environment(&environment(&values))
            .expect("the complete Smoll block resolves");
        assert_eq!(resolved.url, SMOLL_URL);
        assert_eq!(resolved.model, SMOLL_MODEL);
        assert_eq!(resolved.api_key, "smoll-key");
        assert_eq!(resolved.auth(), Auth::Anthropic("smoll-key".to_string()));
    }

    /// Unauthorized/forbidden boundary: the Mid and Giga coordinates, including
    /// their keys, auth switches and endpoints, cannot satisfy a missing Smoll
    /// coordinate -- only the canonical Smoll variables are consulted.
    #[test]
    fn when_configuring_given_other_provider_credentials_without_smoll_credentials_then_refuses_provider_fallback()
     {
        let values = [
            ("LLM_MID_URL", "https://mid.test/v1/chat/completions"),
            ("LLM_MID_MODEL", "mid-model"),
            ("LLM_MID_API_KEY", "mid-key"),
            ("LLM_MID_AUTH", "bearer"),
            ("LLM_GIGA_URL", "https://giga.test/v1/responses"),
            ("LLM_GIGA_MODEL", "giga-model"),
            ("LLM_GIGA_API_KEY", "giga-key"),
            ("LLM_GIGA_AUTH", "none"),
        ];

        let error = smoll_error(&values);
        assert!(error.contains("LLM_SMOLL_MODEL") || error.contains("LLM_SMOLL_API_KEY"));
        assert!(!error.contains("mid-model"), "{error}");
        assert!(!error.contains("giga-model"), "{error}");
    }

    /// Core entry-point behavior: each canonical price variable is bound to its
    /// own provider, and no provider inherits another provider's rate.
    #[test]
    fn when_resolving_provider_rates_given_canonical_variables_then_keeps_each_rate_on_its_provider()
     {
        let values = priced_environment(Some("1.25"), Some("2.5"), Some("3.75"));

        let mid = mid_from_environment(&environment(&values)).expect("the mid block resolves");
        let giga = giga_from_environment(&environment(&values))
            .expect("the giga block resolves")
            .expect("the giga block is configured");
        let smoll =
            smoll_from_environment(&environment(&values)).expect("the smoll block resolves");

        assert_eq!(mid.input_price_per_million_usd, Some(1.25));
        assert_eq!(giga.input_price_per_million_usd, Some(2.5));
        assert_eq!(smoll.input_price_per_million_usd, Some(3.75));

        assert_eq!(
            resolved_rates(&priced_environment(Some("1.25"), None, None)),
            (Some(1.25), None, None)
        );
        assert_eq!(
            resolved_rates(&priced_environment(None, Some("2.5"), None)),
            (None, Some(2.5), None)
        );
        assert_eq!(
            resolved_rates(&priced_environment(None, None, Some("3.75"))),
            (None, None, Some(3.75))
        );
    }

    /// Edge-case table: absent, empty and whitespace-only rates stay missing,
    /// while an explicit zero or a padded number is a configured rate.
    #[test]
    fn when_resolving_provider_rates_given_absent_blank_zero_and_whitespace_values_then_preserves_optional_state()
     {
        assert_eq!(
            resolved_rates(&priced_environment(None, None, None)),
            (None, None, None)
        );

        for blank in ["", "   ", "\t \n"] {
            assert_eq!(
                resolved_rates(&priced_environment(Some(blank), Some(blank), Some(blank))),
                (None, None, None),
                "blank {blank:?}"
            );
        }

        assert_eq!(
            resolved_rates(&priced_environment(Some("0"), Some("0"), Some("0"))),
            (Some(0.0), Some(0.0), Some(0.0))
        );

        assert_eq!(
            resolved_rates(&priced_environment(
                Some(" 1.25 "),
                Some("\t2.5\n"),
                Some(" 3.75 ")
            )),
            (Some(1.25), Some(2.5), Some(3.75))
        );
    }

    /// Minimum valid boundary: zero is a configured rate, not a missing one.
    #[test]
    fn when_resolving_provider_rates_given_zero_then_accepts_the_minimum_non_negative_rate() {
        for zero in ["0", "0.0", " 0.0 "] {
            assert_eq!(
                resolved_rates(&priced_environment(Some(zero), Some(zero), Some(zero))),
                (Some(0.0), Some(0.0), Some(0.0)),
                "zero {zero:?}"
            );
        }
    }

    /// Upper boundary: the largest finite f64 is carried through unchanged.
    #[test]
    fn when_resolving_provider_rates_given_maximum_finite_value_then_preserves_the_finite_rate() {
        let maximum = "1.7976931348623157e308";

        assert_eq!(
            resolved_rates(&priced_environment(
                Some(maximum),
                Some(maximum),
                Some(maximum)
            )),
            (Some(f64::MAX), Some(f64::MAX), Some(f64::MAX))
        );
    }

    /// Malformed/invalid payload: every nonblank value that is not a finite,
    /// non-negative number is refused with the exact variable-naming message.
    #[test]
    fn when_resolving_provider_rates_given_malformed_negative_or_non_finite_values_then_names_the_exact_variable()
     {
        for variable in [
            "LLM_MID_INPUT_PRICE_PER_MILLION_USD",
            "LLM_GIGA_INPUT_PRICE_PER_MILLION_USD",
            "LLM_SMOLL_INPUT_PRICE_PER_MILLION_USD",
        ] {
            for invalid in ["abc", "-1", "-0.5", "NaN", "inf", "-inf"] {
                let values = match variable {
                    "LLM_MID_INPUT_PRICE_PER_MILLION_USD" => {
                        priced_environment(Some(invalid), None, None)
                    }
                    "LLM_GIGA_INPUT_PRICE_PER_MILLION_USD" => {
                        priced_environment(None, Some(invalid), None)
                    }
                    _ => priced_environment(None, None, Some(invalid)),
                };
                let error = match variable {
                    "LLM_MID_INPUT_PRICE_PER_MILLION_USD" => {
                        mid_from_environment(&environment(&values))
                            .err()
                            .unwrap_or_else(|| panic!("mid must refuse {invalid:?}"))
                    }
                    "LLM_GIGA_INPUT_PRICE_PER_MILLION_USD" => {
                        giga_from_environment(&environment(&values))
                            .err()
                            .unwrap_or_else(|| panic!("giga must refuse {invalid:?}"))
                    }
                    _ => smoll_from_environment(&environment(&values))
                        .err()
                        .unwrap_or_else(|| panic!("smoll must refuse {invalid:?}")),
                };
                assert_eq!(
                    error,
                    format!(
                        "{variable} must be a finite, non-negative number of USD per million input tokens."
                    ),
                    "variable {variable} value {invalid:?}"
                );
            }
        }
    }
}
