//! The one configuration type: the resolved provider choice -- an
//! [`llm::Mode`], a mid provider block, an optional giga provider block -- and
//! the transport every request goes through. Resolved from the environment by
//! the binary, or built from explicit values by anything that is not the CLI.

use crate::api;
use crate::llm;
use crate::status;
use crate::tools;
use std::sync::Arc;

#[derive(Clone)]
pub struct Config {
    mode: llm::Mode,
    mid: llm::factory::ProviderSettings,
    giga: Option<llm::factory::ProviderSettings>,
    smoll: Option<llm::factory::SmollSettings>,
    transport: Arc<dyn api::Transport>,
}

/// The two-line fatal message for a remote endpoint with no key: the first line
/// names the role and the endpoint, the second lists that role's own variables.
fn missing_key_message(url: &str, role: &str) -> String {
    let hint = match role {
        "giga" => "Set LLM_GIGA_API_KEY, or set LLM_GIGA_AUTH=none for a keyless endpoint.",
        _ => "Set LLM_MID_API_KEY, or set LLM_MID_AUTH=none for a keyless endpoint.",
    };
    format!("[ERROR] No API key configured for the remote {role} endpoint {url}.\n{hint}")
}

/// The one fatal shape: print the `[ERROR] ` line and leave with status 1, so
/// every configuration the process cannot serve fails the same way.
fn fatal(message: &str) -> ! {
    let _ = status::shared().stderr_log(&format!("[ERROR] {message}"));
    std::process::exit(1);
}

impl Config {
    /// Resolve the LLM configuration from the process environment (and the
    /// `.env` file). `AGENT_MODE` picks the canonical `mid`, `giga` or
    /// `hybrid` mode; the `LLM_GIGA_*` block, when set, becomes the giga
    /// provider, and the required `LLM_SMOLL_*` coordinates become the one
    /// provider Summary always uses. A configuration the process cannot serve --
    /// an unknown mode, a half-written giga block, a mode that needs giga
    /// without it, or missing Smoll model/key -- prints one `[ERROR] ...` line
    /// and exits 1 instead of falling back silently.
    pub fn from_env() -> Config {
        Config::from_environment(&llm::factory::env_value).unwrap_or_else(|message| fatal(&message))
    }

    /// Resolve the LLM configuration from `environment`, the same lookup by
    /// canonical variable name that [`from_env`](Config::from_env) reads from
    /// the process. The resolved value is returned instead of leaving the
    /// process, so the whole environment-to-settings binding -- the mode, both
    /// provider blocks and the routing they produce -- can be asserted
    /// deterministically against an isolated table, with no `.env` and no
    /// process-environment mutation. The error is the exact fatal message
    /// `from_env` prints.
    pub fn from_environment(environment: llm::factory::Environment<'_>) -> Result<Config, String> {
        let mode = llm::factory::mode_from_environment(environment)?;

        // Read the two required giga coordinates raw, so a block carrying only
        // one of them is caught here rather than collapsing to "absent".
        let giga_url = environment("LLM_GIGA_URL");
        let giga_model = environment("LLM_GIGA_MODEL");
        if let Some(missing) =
            llm::factory::missing_giga_variable(giga_url.as_deref(), giga_model.as_deref())
        {
            return Err(format!(
                "The giga provider is half-configured: {missing} is not set. Set it, or unset the other one."
            ));
        }

        let giga = llm::factory::giga_from_environment(environment)?;
        if let Some(message) = llm::factory::giga_requirement(mode, giga.is_some()) {
            return Err(message);
        }

        let smoll = llm::factory::smoll_from_environment(environment)?;
        let mid = llm::factory::mid_from_environment(environment)?;

        Ok(Config {
            mode,
            mid,
            giga,
            smoll: Some(smoll),
            transport: Arc::new(api::HttpTransport),
        })
    }

    /// Build a configuration from explicit values, for callers that are not the
    /// CLI (the integration tests). Auth stays enabled -- a key is sent when one
    /// is given, exactly as `from_env` would with the key set -- and no thinking
    /// override is requested, so the provider's own default applies. The mode is
    /// `Mid`, non-summary phases run on the mid provider, and no Smoll block is
    /// configured: a caller that reaches the Summary phase must add one with
    /// [`Config::with_smoll`].
    pub fn new(
        url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Config {
        Config {
            mode: llm::Mode::Mid,
            mid: llm::factory::ProviderSettings {
                url: url.into(),
                model: model.into(),
                api_key,
                auth_disabled: false,
                reasoning_effort: None,
                input_price_per_million_usd: None,
                cached_input_price_per_million_usd: None,
                output_price_per_million_usd: None,
            },
            giga: None,
            smoll: None,
            transport: Arc::new(api::HttpTransport),
        }
    }

    /// Replace the transport this configuration sends through. The CLI never
    /// calls this; it exists so tests can route the agent loops to recorded
    /// exchanges instead of a live provider.
    pub fn with_transport(mut self, transport: Arc<dyn api::Transport>) -> Config {
        self.transport = transport;
        self
    }

    /// Choose how the available providers are combined. Assembling every mode
    /// with [`Config::with_giga`] lets a caller set one up with no environment
    /// at all.
    pub fn with_mode(mut self, mode: llm::Mode) -> Config {
        self.mode = mode;
        self
    }

    /// Test-only: set the mid provider's reasoning effort explicitly. A
    /// provider-boundary test uses this to grow the encoded request body
    /// without touching the transcript or the model the status line prints.
    /// Production keeps reading effort from the environment and from
    /// [`Config::new`]'s unchanged defaults.
    #[cfg(test)]
    pub(crate) fn with_mid_reasoning_effort(mut self, effort: impl Into<String>) -> Config {
        self.mid.reasoning_effort = Some(effort.into());
        self
    }

    /// Add the giga provider block. Auth stays enabled and no thinking override
    /// is requested, mirroring [`Config::new`].
    pub fn with_giga(
        mut self,
        url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Config {
        self.giga = Some(llm::factory::ProviderSettings {
            url: url.into(),
            model: model.into(),
            api_key,
            auth_disabled: false,
            reasoning_effort: None,
            input_price_per_million_usd: None,
            cached_input_price_per_million_usd: None,
            output_price_per_million_usd: None,
        });
        self
    }

    /// Add the dedicated Smoll provider block Summary always uses. The API key
    /// is required: this endpoint has no keyless mode and no auth-disable
    /// switch, so a blank key is rejected here, loudly, rather than becoming a
    /// Summary request that carries no authentication. No Mid or Giga value is
    /// copied into the block.
    pub fn with_smoll(
        mut self,
        url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Config {
        self.smoll = Some(
            llm::factory::SmollSettings::new(url, model, api_key)
                .unwrap_or_else(|message| panic!("{message}")),
        );
        self
    }

    /// The provider that serves `phase`. Summary is routed before the mode is
    /// consulted: it always uses the dedicated Smoll Anthropic Messages
    /// provider, in every mode, with no fallback to Mid or Giga. Every other
    /// phase keeps the mode's routing.
    ///
    /// In giga and hybrid mode a missing giga block is a programming error: the
    /// mode cannot run without one. A missing Smoll block is the same kind of
    /// error for Summary.
    ///
    /// Hybrid mode splits the non-summary phases by what they do, not by where
    /// they sit in the run: the giga provider designs and audits (plan, spec,
    /// tickets, triage, review) and the mid provider runs the work (code).
    pub fn provider(&self, phase: tools::Phase) -> Box<dyn llm::LlmProvider> {
        if phase == tools::Phase::Summary {
            return llm::factory::anthropic_provider(self.smoll_settings());
        }

        match self.mode {
            llm::Mode::Mid => llm::factory::chat_provider(self.mid.clone()),
            llm::Mode::Giga => llm::factory::responses_provider(self.giga_settings()),
            llm::Mode::Hybrid => match phase {
                tools::Phase::Code => llm::factory::chat_provider(self.mid.clone()),
                tools::Phase::Plan
                | tools::Phase::Spec
                | tools::Phase::Tickets
                | tools::Phase::Triage
                | tools::Phase::Review => llm::factory::responses_provider(self.giga_settings()),
                tools::Phase::Summary => {
                    unreachable!("Summary is routed before the mode is consulted")
                }
            },
        }
    }

    /// The canonical lowercase role name of the provider that serves `phase`:
    /// `smoll` for Summary, `mid` or `giga` for every other phase.
    ///
    /// This mirrors [`Config::provider`]'s routing exactly -- the Summary route
    /// first, then the mode split and the phase split inside hybrid mode -- so
    /// the name can never disagree with the provider the request actually goes
    /// to. The label names the provider serving one request, not the configured
    /// mode, so hybrid mode reports `giga`, `mid` or `smoll` per phase and never
    /// `hybrid`.
    ///
    /// The agent loop resolves this once per turn and forwards it to the
    /// thinking status line for every request attempt.
    pub(crate) fn provider_role(&self, phase: tools::Phase) -> &'static str {
        if phase == tools::Phase::Summary {
            return "smoll";
        }

        match self.mode {
            llm::Mode::Mid => "mid",
            llm::Mode::Giga => "giga",
            llm::Mode::Hybrid => match phase {
                tools::Phase::Code => "mid",
                tools::Phase::Plan
                | tools::Phase::Spec
                | tools::Phase::Tickets
                | tools::Phase::Triage
                | tools::Phase::Review => "giga",
                tools::Phase::Summary => {
                    unreachable!("Summary is routed before the mode is consulted")
                }
            },
        }
    }

    /// The configured model identifier of the provider that serves `phase`,
    /// selected by exactly the same routing as [`Config::provider`] and
    /// [`Config::provider_role`]: the dedicated Smoll model for Summary, the
    /// mode's own model for every non-summary phase, and within hybrid mode the
    /// Mid model for Code and the Giga model for the planning and audit phases.
    ///
    /// The name names the provider serving one request, not the configured
    /// mode, so hybrid mode never reports a `hybrid` model. Missing Giga or
    /// Smoll settings are the same programming errors the provider route
    /// already treats them as.
    pub(crate) fn provider_model(&self, phase: tools::Phase) -> &str {
        if phase == tools::Phase::Summary {
            return self.smoll_model();
        }

        match self.mode {
            llm::Mode::Mid => &self.mid.model,
            llm::Mode::Giga => self.giga_model(),
            llm::Mode::Hybrid => match phase {
                tools::Phase::Code => &self.mid.model,
                tools::Phase::Plan
                | tools::Phase::Spec
                | tools::Phase::Tickets
                | tools::Phase::Triage
                | tools::Phase::Review => self.giga_model(),
                tools::Phase::Summary => {
                    unreachable!("Summary is routed before the mode is consulted")
                }
            },
        }
    }

    /// The optional input price of the provider that serves `phase`, selected
    /// by exactly the same canonical routing as [`Config::provider`] and
    /// [`Config::provider_role`]: the dedicated Smoll rate for Summary, the
    /// mode's own rate for every non-summary phase, and within hybrid mode the
    /// Mid rate for Code and the Giga rate for the planning and audit phases.
    ///
    /// `None` means the selected provider itself is unpriced; another
    /// provider's configured rate is never substituted. Missing Giga or Smoll
    /// settings remain the same programming errors the provider route already
    /// treats them as, so pricing never becomes a routing fallback.
    pub fn input_price(&self, phase: tools::Phase) -> Option<f64> {
        match self.provider_role(phase) {
            "smoll" => self.smoll_settings().input_price_per_million_usd,
            "mid" => self.mid.input_price_per_million_usd,
            "giga" => self.giga_settings().input_price_per_million_usd,
            role => unreachable!("{role} is not a provider role"),
        }
    }

    /// The optional cached-input price of the provider that serves `phase`,
    /// selected by exactly the same canonical routing as
    /// [`Config::input_price`]: the Smoll rate for Summary, the mode's own rate
    /// for every non-summary phase, and within hybrid mode the Mid rate for Code
    /// and the Giga rate for the planning and audit phases.
    ///
    /// `None` means the selected provider itself is unpriced; no other
    /// provider's cached-input rate is substituted.
    pub fn cached_input_price(&self, phase: tools::Phase) -> Option<f64> {
        match self.provider_role(phase) {
            "smoll" => self.smoll_settings().cached_input_price_per_million_usd,
            "mid" => self.mid.cached_input_price_per_million_usd,
            "giga" => self.giga_settings().cached_input_price_per_million_usd,
            role => unreachable!("{role} is not a provider role"),
        }
    }

    /// The optional output price of the provider that serves `phase`, selected
    /// by exactly the same canonical routing as [`Config::input_price`]: the
    /// Smoll rate for Summary, the mode's own rate for every non-summary phase,
    /// and within hybrid mode the Mid rate for Code and the Giga rate for the
    /// planning and audit phases.
    ///
    /// `None` means the selected provider itself is unpriced; no other
    /// provider's output rate is substituted.
    pub fn output_price(&self, phase: tools::Phase) -> Option<f64> {
        match self.provider_role(phase) {
            "smoll" => self.smoll_settings().output_price_per_million_usd,
            "mid" => self.mid.output_price_per_million_usd,
            "giga" => self.giga_settings().output_price_per_million_usd,
            role => unreachable!("{role} is not a provider role"),
        }
    }

    /// Configuration for a ticket's second (retry) code-agent attempt: a hybrid
    /// configuration with a giga block escalates to giga, so the retry's code
    /// phase runs on the responses provider with the giga role, model and
    /// configured rate. A mid or giga configuration -- and a hybrid
    /// configuration without a giga block -- keeps its mode, so it never
    /// selects a provider it does not have.
    ///
    /// Only the mode changes: both provider blocks, the optional prices, every
    /// credential and reasoning setting, the Smoll block and the transport are
    /// carried over unchanged, and the configuration this is derived from keeps
    /// its own routing. Summary is routed before the mode is consulted, so it
    /// stays on the Smoll Anthropic Messages provider in the derived
    /// configuration too.
    ///
    /// The ticket lifecycle's retry attempt selects this configuration; the
    /// first attempt always keeps the original routing.
    pub(crate) fn code_retry_config(&self) -> Config {
        let mut retry = self.clone();
        if retry.mode == llm::Mode::Hybrid && retry.giga.is_some() {
            retry.mode = llm::Mode::Giga;
        }
        retry
    }

    /// The giga configured model, or the fixed panic that says the mode needs a
    /// giga block.
    fn giga_model(&self) -> &str {
        &self
            .giga
            .as_ref()
            .expect("the giga provider is required by the mode")
            .model
    }

    /// The dedicated Smoll configured model, or the fixed panic that says
    /// Summary needs a Smoll block.
    fn smoll_model(&self) -> &str {
        &self
            .smoll
            .as_ref()
            .expect("the smoll provider is required for the summary phase")
            .model
    }

    /// The giga block, or the fixed panic that says the mode needs one.
    fn giga_settings(&self) -> llm::factory::ProviderSettings {
        self.giga
            .clone()
            .expect("the giga provider is required by the mode")
    }

    /// The Smoll block, or the fixed panic that says Summary needs one. There is
    /// deliberately no fallback: a configuration without Smoll cannot serve the
    /// summary pass on Mid or Giga.
    fn smoll_settings(&self) -> llm::factory::SmollSettings {
        self.smoll
            .clone()
            .expect("the smoll provider is required for the summary phase")
    }

    /// Send one already-encoded request through the configured transport, so
    /// the loop and the wire tests never touch a field.
    pub fn send(&self, outgoing: &api::Outgoing) -> Result<api::RawResponse, api::CallError> {
        self.transport.send(outgoing)
    }

    /// One line naming the mode and, for every phase, the route it takes and
    /// the model that serves it -- so the operator reads the shipped routing
    /// shape, not a summary of it. Summary is always the Smoll Anthropic
    /// Messages route, in every mode.
    pub fn describe(&self) -> String {
        let smoll = self.smoll_settings();
        match self.mode {
            llm::Mode::Mid => format!(
                "mode mid: plan/spec/tickets/triage/code/review -> chat-completions ({}), \
                 summary -> anthropic-messages ({})",
                self.mid.model, smoll.model
            ),
            llm::Mode::Giga => format!(
                "mode giga: plan/spec/tickets/triage/code/review -> responses ({}), \
                 summary -> anthropic-messages ({})",
                self.giga_settings().model,
                smoll.model
            ),
            llm::Mode::Hybrid => format!(
                "mode hybrid: plan/spec/tickets/triage/review -> responses ({}), code -> \
                 chat-completions ({}), summary -> anthropic-messages ({})",
                self.giga_settings().model,
                self.mid.model,
                smoll.model
            ),
        }
    }

    /// A configuration without a Smoll block cannot serve Summary at all, so
    /// startup refuses it loudly rather than substituting Mid or Giga. A remote
    /// https endpoint without a key (and without auth explicitly disabled) is
    /// fatal too: fail fast instead of silently falling back elsewhere. Every
    /// provider the mode actually uses is checked, named by its canonical role --
    /// the mid provider first in hybrid mode.
    pub fn validate(&self) {
        if self.smoll.is_none() {
            fatal(
                "The summary phase needs the smoll provider: set LLM_SMOLL_MODEL and \
                 LLM_SMOLL_API_KEY for the Anthropic Messages endpoint.",
            );
        }

        match self.mode {
            llm::Mode::Mid => self.validate_role(&self.mid, "mid"),
            llm::Mode::Giga => self.validate_role(&self.giga_settings(), "giga"),
            llm::Mode::Hybrid => {
                self.validate_role(&self.mid, "mid");
                self.validate_role(&self.giga_settings(), "giga");
            }
        }
    }

    /// The fatal-key check for one provider block: when the endpoint is remote
    /// and no key is configured, print exactly one message -- the role and the
    /// endpoint on the first line, the role's own variables on the second --
    /// and exit.
    fn validate_role(&self, settings: &llm::factory::ProviderSettings, role: &str) {
        let is_https = settings
            .url
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("https://");
        if is_https && settings.api_key.is_none() && !settings.auth_disabled {
            let _ = status::shared().stderr_log(&missing_key_message(&settings.url, role));
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Auth, Message};
    use crate::llm::Mode;
    use crate::tools::Phase;
    use serde_json::json;

    const MID_URL: &str = "http://mid.test/v1/chat/completions";
    const MID_MODEL: &str = "mid-model";
    const GIGA_URL: &str = "https://giga.test/v1/responses";
    const GIGA_MODEL: &str = "giga-model";
    const SMOLL_URL: &str = "https://smoll.test/v1/messages";
    const SMOLL_MODEL: &str = "smoll-model";

    const PHASES: [Phase; 7] = [
        Phase::Plan,
        Phase::Spec,
        Phase::Tickets,
        Phase::Triage,
        Phase::Code,
        Phase::Review,
        Phase::Summary,
    ];

    fn mid_config() -> Config {
        Config::new(MID_URL, MID_MODEL, Some("mid-key".to_string())).with_smoll(
            SMOLL_URL,
            SMOLL_MODEL,
            "smoll-key",
        )
    }

    fn giga_config() -> Config {
        mid_config().with_giga(GIGA_URL, GIGA_MODEL, Some("giga-key".to_string()))
    }

    /// The top of the documented effort range.
    const MAX_EFFORT: &str = "max";

    /// An isolated environment: a table consulted by canonical variable name.
    /// The process environment is neither read nor mutated, so the whole
    /// binding is deterministic and safe under parallel test threads.
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
    fn resolution_error(values: &[(&'static str, &'static str)]) -> String {
        match Config::from_environment(&environment(values)) {
            Ok(_) => panic!("the environment must not resolve: {values:?}"),
            Err(message) => message,
        }
    }

    /// Every canonical provider variable in one environment: both blocks carry
    /// a URL, model, key, auth switch and effort.
    fn canonical_environment(
        mode: &'static str,
        mid_effort: &'static str,
        giga_effort: &'static str,
    ) -> Vec<(&'static str, &'static str)> {
        vec![
            ("AGENT_MODE", mode),
            ("LLM_MID_URL", MID_URL),
            ("LLM_MID_MODEL", MID_MODEL),
            ("LLM_MID_API_KEY", "sk-mid"),
            ("LLM_MID_AUTH", "bearer"),
            ("LLM_MID_REASONING_EFFORT", mid_effort),
            ("LLM_GIGA_URL", GIGA_URL),
            ("LLM_GIGA_MODEL", GIGA_MODEL),
            ("LLM_GIGA_API_KEY", "sk-giga"),
            ("LLM_GIGA_AUTH", "none"),
            ("LLM_GIGA_REASONING_EFFORT", giga_effort),
            ("LLM_SMOLL_URL", SMOLL_URL),
            ("LLM_SMOLL_MODEL", SMOLL_MODEL),
            ("LLM_SMOLL_API_KEY", "sk-smoll"),
        ]
    }

    /// A complete Smoll block on its own, for the regression cases that must
    /// supply it while testing another provider's diagnostics.
    fn smoll_environment() -> Vec<(&'static str, &'static str)> {
        vec![
            ("LLM_SMOLL_URL", SMOLL_URL),
            ("LLM_SMOLL_MODEL", SMOLL_MODEL),
            ("LLM_SMOLL_API_KEY", "sk-smoll"),
        ]
    }

    /// The established route for one phase under one mode: provider name and
    /// provider role.
    fn expected_route(mode: Mode, phase: Phase) -> (&'static str, &'static str) {
        match (mode, phase) {
            (_, Phase::Summary) => ("anthropic-messages", "smoll"),
            (Mode::Mid, _) => ("chat-completions", "mid"),
            (Mode::Giga, _) => ("responses", "giga"),
            (Mode::Hybrid, Phase::Code) => ("chat-completions", "mid"),
            (Mode::Hybrid, _) => ("responses", "giga"),
        }
    }

    /// Assert every phase keeps its established route under the config's mode.
    fn assert_routes(config: &Config) {
        for phase in PHASES {
            let (provider, role) = expected_route(config.mode, phase);
            assert_eq!(config.provider(phase).name(), provider, "phase {phase:?}");
            assert_eq!(config.provider_role(phase), role, "phase {phase:?}");
        }
    }

    /// Assert the Smoll contract through the public surface only: the Summary
    /// provider names the configured endpoint and model, authenticates with the
    /// configured key, and the description reports the same model on the
    /// Anthropic Messages route.
    fn assert_summary_settings(config: &Config, url: &str, model: &str, api_key: &str) {
        let provider = config.provider(Phase::Summary);
        assert_eq!(provider.name(), "anthropic-messages");
        assert_eq!(config.provider_role(Phase::Summary), "smoll");

        let prepared = provider.prepare(&[Message::user("summarize the change")], Phase::Summary);
        assert_eq!(prepared.outgoing.url, url);
        assert_eq!(prepared.outgoing.body["model"], json!(model));
        assert_eq!(prepared.outgoing.auth, Auth::Anthropic(api_key.to_string()));

        let description = config.describe();
        assert!(
            description.contains(&format!("summary -> anthropic-messages ({model})")),
            "description does not expose the Smoll summary route: {description}"
        );
    }

    /// Assert one phase's route through the public surface only: wire protocol,
    /// role, model, encoded model and optional input rate.
    fn assert_phase_route(
        config: &Config,
        phase: Phase,
        protocol: &str,
        role: &str,
        model: &str,
        price: Option<f64>,
    ) {
        let provider = config.provider(phase);
        assert_eq!(provider.name(), protocol, "phase {phase:?} protocol");
        assert_eq!(config.provider_role(phase), role, "phase {phase:?} role");
        assert_eq!(config.provider_model(phase), model, "phase {phase:?} model");
        assert_eq!(
            provider
                .prepare(&[Message::user("hi")], phase)
                .outgoing
                .body["model"],
            json!(model),
            "phase {phase:?} wire model"
        );
        assert_eq!(config.input_price(phase), price, "phase {phase:?} price");
    }

    /// A transport double at the external HTTP boundary: it records the requests
    /// it is handed and answers with an empty success, so a test observes which
    /// configuration sent through it without touching the network.
    #[derive(Default)]
    struct RecordingTransport {
        sent: std::sync::Mutex<Vec<(String, Auth, serde_json::Value)>>,
    }

    impl api::Transport for RecordingTransport {
        fn send(&self, outgoing: &api::Outgoing) -> Result<api::RawResponse, api::CallError> {
            self.sent
                .lock()
                .expect("the recording transport is never poisoned")
                .push((
                    outgoing.url.clone(),
                    outgoing.auth.clone(),
                    outgoing.body.clone(),
                ));
            Ok(api::RawResponse {
                status: 200,
                body: "{}".to_string(),
            })
        }
    }

    /// Core entry-point behavior: Summary is the one phase that leaves the mode's
    /// routing and runs on the separately configured Smoll provider. Every other
    /// phase keeps its protocol, endpoint, model and role -- mid is all
    /// chat-completions, giga is all responses, and hybrid sends code to mid
    /// while its planning and audit phases stay on giga.
    #[test]
    fn when_routing_given_each_agent_mode_then_sends_only_summary_to_smoll() {
        let configurations = [
            (Mode::Mid, mid_config()),
            (Mode::Giga, giga_config().with_mode(Mode::Giga)),
            (Mode::Hybrid, giga_config().with_mode(Mode::Hybrid)),
        ];

        for (mode, config) in configurations {
            for phase in PHASES {
                let provider = config.provider(phase);
                let prepared = provider.prepare(&[Message::user("hi")], phase);

                if phase == Phase::Summary {
                    assert_eq!(
                        provider.name(),
                        "anthropic-messages",
                        "mode {mode:?} summary protocol"
                    );
                    assert_eq!(
                        config.provider_role(phase),
                        "smoll",
                        "mode {mode:?} summary role"
                    );
                    assert_eq!(
                        prepared.outgoing.url, SMOLL_URL,
                        "mode {mode:?} summary endpoint"
                    );
                    assert_eq!(
                        prepared.outgoing.body["model"],
                        json!(SMOLL_MODEL),
                        "mode {mode:?} summary model"
                    );
                    continue;
                }

                let (protocol, role, url, model) = match (mode, phase) {
                    (Mode::Mid, _) => ("chat-completions", "mid", MID_URL, MID_MODEL),
                    (Mode::Giga, _) => ("responses", "giga", GIGA_URL, GIGA_MODEL),
                    (Mode::Hybrid, Phase::Code) => ("chat-completions", "mid", MID_URL, MID_MODEL),
                    (Mode::Hybrid, _) => ("responses", "giga", GIGA_URL, GIGA_MODEL),
                };
                assert_eq!(provider.name(), protocol, "mode {mode:?} phase {phase:?}");
                assert_eq!(
                    config.provider_role(phase),
                    role,
                    "mode {mode:?} phase {phase:?}"
                );
                assert_eq!(
                    prepared.outgoing.url, url,
                    "mode {mode:?} phase {phase:?} endpoint"
                );
                assert_eq!(
                    prepared.outgoing.body["model"],
                    json!(model),
                    "mode {mode:?} phase {phase:?} model"
                );
            }

            for phase in PHASES {
                assert_ne!(
                    config.provider_role(phase),
                    "hybrid",
                    "the mode's own name is never a role: {phase:?}"
                );
            }
        }
    }

    /// Giga and hybrid modes without a giga block are a programming error with
    /// one fixed message.
    #[test]
    #[should_panic(expected = "the giga provider is required by the mode")]
    fn when_routing_given_giga_mode_without_a_giga_block_then_panics() {
        let _ = mid_config().with_mode(Mode::Giga).provider(Phase::Plan);
    }

    /// The hybrid code phase needs no giga, but the giga phases do.
    #[test]
    #[should_panic(expected = "the giga provider is required by the mode")]
    fn when_routing_given_hybrid_mode_without_a_giga_block_then_panics_for_a_giga_phase() {
        let _ = mid_config().with_mode(Mode::Hybrid).provider(Phase::Plan);
    }

    /// The mid description names every non-summary route and the mid model, then
    /// names Summary on the Smoll Messages route with the Smoll model.
    #[test]
    fn when_describing_given_mid_then_names_chat_completions_and_the_smoll_summary_model() {
        assert_eq!(
            mid_config().describe(),
            "mode mid: plan/spec/tickets/triage/code/review -> chat-completions (mid-model), \
             summary -> anthropic-messages (smoll-model)"
        );
    }

    /// The giga description names every non-summary route and the giga model,
    /// then names Summary on the Smoll Messages route with the Smoll model.
    #[test]
    fn when_describing_given_giga_then_names_responses_and_the_smoll_summary_model() {
        assert_eq!(
            giga_config().with_mode(Mode::Giga).describe(),
            "mode giga: plan/spec/tickets/triage/code/review -> responses (giga-model), \
             summary -> anthropic-messages (smoll-model)"
        );
    }

    /// The hybrid description names both non-summary halves and their models,
    /// then names Summary on the Smoll Messages route with the Smoll model.
    #[test]
    fn when_describing_given_hybrid_then_names_both_halves_and_the_smoll_summary_model() {
        assert_eq!(
            giga_config().with_mode(Mode::Hybrid).describe(),
            "mode hybrid: plan/spec/tickets/triage/review -> responses (giga-model), \
             code -> chat-completions (mid-model), \
             summary -> anthropic-messages (smoll-model)"
        );
    }

    /// Contract: every mode's description identifies Summary as smoll on the
    /// Anthropic Messages protocol with the configured Smoll model, and never
    /// describes Summary as Mid or Giga, while the non-summary routes stay as
    /// they were.
    #[test]
    fn when_describing_given_each_agent_mode_then_names_the_smoll_summary_model_and_preserves_other_routes()
     {
        let configurations = [
            (Mode::Mid, mid_config()),
            (Mode::Giga, giga_config().with_mode(Mode::Giga)),
            (Mode::Hybrid, giga_config().with_mode(Mode::Hybrid)),
        ];

        for (mode, config) in configurations {
            let description = config.describe();
            assert!(
                description.contains("summary -> anthropic-messages (smoll-model)"),
                "mode {mode:?}: {description}"
            );
            assert!(
                !description.contains(&format!("summary -> chat-completions ({MID_MODEL})")),
                "mode {mode:?} must not describe Summary as Mid: {description}"
            );
            assert!(
                !description.contains(&format!("summary -> responses ({GIGA_MODEL})")),
                "mode {mode:?} must not describe Summary as Giga: {description}"
            );

            match mode {
                Mode::Mid => assert!(description.starts_with(
                    "mode mid: plan/spec/tickets/triage/code/review -> chat-completions (mid-model)"
                )),
                Mode::Giga => assert!(description.starts_with(
                    "mode giga: plan/spec/tickets/triage/code/review -> responses (giga-model)"
                )),
                Mode::Hybrid => assert!(description.starts_with(
                    "mode hybrid: plan/spec/tickets/triage/review -> responses (giga-model), code -> chat-completions (mid-model)"
                )),
            }
        }
    }

    /// The mid fatal message names the mid role and the mid variables.
    #[test]
    fn when_validating_given_a_keyless_https_mid_then_the_message_names_mid_variables() {
        assert_eq!(
            missing_key_message("https://mid.test/v1/chat/completions", "mid"),
            "[ERROR] No API key configured for the remote mid endpoint \
             https://mid.test/v1/chat/completions.\n\
             Set LLM_MID_API_KEY, or set LLM_MID_AUTH=none for a keyless endpoint."
        );
    }

    /// The giga fatal message names the giga role and variables.
    #[test]
    fn when_validating_given_a_keyless_https_giga_then_the_message_names_giga_variables() {
        assert_eq!(
            missing_key_message("https://giga.test/v1/responses", "giga"),
            "[ERROR] No API key configured for the remote giga endpoint \
             https://giga.test/v1/responses.\n\
             Set LLM_GIGA_API_KEY, or set LLM_GIGA_AUTH=none for a keyless endpoint."
        );
    }

    /// Explicit keyed mid settings default to mid mode with the given
    /// coordinates, auth enabled, no reasoning effort, no giga block and the
    /// separately configured Smoll summary block.
    #[test]
    fn when_configuring_given_keyed_mid_settings_then_defaults_to_mid_without_internal_name() {
        let config = mid_config();

        assert_eq!(config.mode, Mode::Mid);
        assert_eq!(config.mid.url, MID_URL);
        assert_eq!(config.mid.model, MID_MODEL);
        assert_eq!(config.mid.api_key.as_deref(), Some("mid-key"));
        assert!(!config.mid.auth_disabled);
        assert_eq!(config.mid.reasoning_effort, None);
        assert!(config.giga.is_none());

        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Triage,
            Phase::Code,
            Phase::Review,
        ] {
            assert_eq!(config.provider(phase).name(), "chat-completions");
            assert_eq!(config.provider_role(phase), "mid");
        }
        assert_summary_settings(&config, SMOLL_URL, SMOLL_MODEL, "smoll-key");
    }

    /// Explicit mid settings also accept no key: the mid configuration is
    /// keyed or keyless, still defaults to mid mode, and sends no auth.
    #[test]
    fn when_configuring_given_keyless_mid_settings_then_defaults_to_mid_without_internal_name() {
        let config = Config::new(MID_URL, MID_MODEL, None);
        let config = config.with_smoll(SMOLL_URL, SMOLL_MODEL, "smoll-key");

        assert_eq!(config.mode, Mode::Mid);
        assert_eq!(config.mid.url, MID_URL);
        assert_eq!(config.mid.model, MID_MODEL);
        assert_eq!(config.mid.api_key, None);
        assert!(!config.mid.auth_disabled);
        assert_eq!(config.mid.reasoning_effort, None);
        assert!(config.giga.is_none());

        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Triage,
            Phase::Code,
            Phase::Review,
        ] {
            assert_eq!(config.provider(phase).name(), "chat-completions");
            assert_eq!(config.provider_role(phase), "mid");
        }
        assert_eq!(config.provider(Phase::Summary).name(), "anthropic-messages");
        assert_eq!(config.provider_role(Phase::Summary), "smoll");
    }

    /// Explicit Smoll settings expose their endpoint, model and required key
    /// through the public Summary request and the configuration description.
    #[test]
    fn when_configuring_given_explicit_smoll_settings_then_exposes_values_through_summary_requests()
    {
        let config = Config::new(MID_URL, MID_MODEL, Some("mid-key".to_string())).with_smoll(
            SMOLL_URL,
            SMOLL_MODEL,
            "smoll-key",
        );

        assert_summary_settings(&config, SMOLL_URL, SMOLL_MODEL, "smoll-key");
    }

    /// Core entry-point behavior: an explicit Smoll configuration always selects
    /// Anthropic's native authentication with its own configured key, in every
    /// mode, so no summary request can leave unauthenticated.
    #[test]
    fn when_configuring_given_a_required_smoll_key_then_uses_anthropic_authentication() {
        let configurations = [
            (Mode::Mid, mid_config()),
            (Mode::Giga, giga_config().with_mode(Mode::Giga)),
            (Mode::Hybrid, giga_config().with_mode(Mode::Hybrid)),
        ];

        for (mode, config) in configurations {
            let prepared = config
                .provider(Phase::Summary)
                .prepare(&[Message::user("summarize the change")], Phase::Summary);

            assert_eq!(
                prepared.outgoing.auth,
                Auth::Anthropic("smoll-key".to_string()),
                "mode {mode:?}"
            );
            assert_eq!(
                prepared.outgoing.url, SMOLL_URL,
                "mode {mode:?} keeps the configured endpoint"
            );
        }
    }

    /// Edge-case boundary: a whitespace-only explicit key is rejected where the
    /// provider is configured, so it can never become an unauthenticated
    /// summary request.
    #[test]
    #[should_panic(expected = "The smoll provider requires a non-blank API key")]
    fn when_configuring_given_a_blank_smoll_key_then_rejects_the_provider() {
        let _ = Config::new(MID_URL, MID_MODEL, Some("mid-key".to_string())).with_smoll(
            SMOLL_URL,
            SMOLL_MODEL,
            "   ",
        );
    }

    /// An explicitly constructed configuration with no Smoll block has no
    /// fallback: asking for the Summary provider is a programming error, never
    /// the mid or giga provider.
    #[test]
    #[should_panic(expected = "the smoll provider is required for the summary phase")]
    fn when_routing_given_mid_and_giga_credentials_without_smoll_then_refuses_the_summary_provider()
    {
        let config = Config::new(MID_URL, MID_MODEL, Some("mid-key".to_string()))
            .with_giga(GIGA_URL, GIGA_MODEL, Some("giga-key".to_string()))
            .with_mode(Mode::Hybrid);

        let _ = config.provider(Phase::Summary);
    }

    /// Explicit giga settings record provider values with auth enabled and no
    /// effort, without touching the mid block.
    #[test]
    fn when_configuring_given_explicit_giga_settings_then_records_provider_values_without_builder_name()
     {
        let config = giga_config();

        let giga = config.giga.expect("the giga block is recorded");
        assert_eq!(giga.url, GIGA_URL);
        assert_eq!(giga.model, GIGA_MODEL);
        assert_eq!(giga.api_key.as_deref(), Some("giga-key"));
        assert!(!giga.auth_disabled);
        assert_eq!(giga.reasoning_effort, None);

        assert_eq!(config.mid.url, MID_URL);
        assert_eq!(config.mid.model, MID_MODEL);
    }

    /// Contract: directly constructed provider settings, `Config::new`,
    /// `Config::with_giga` and `SmollSettings::new` leave every new rate
    /// unset, and no role inherits another role's price.
    #[test]
    fn when_constructing_provider_settings_given_rates_are_not_supplied_then_all_roles_default_to_unset_prices()
     {
        let direct = llm::factory::ProviderSettings {
            url: MID_URL.to_string(),
            model: MID_MODEL.to_string(),
            api_key: Some("direct-key".to_string()),
            auth_disabled: false,
            reasoning_effort: None,
            input_price_per_million_usd: None,
            cached_input_price_per_million_usd: None,
            output_price_per_million_usd: None,
        };
        assert_eq!(direct.input_price_per_million_usd, None);
        assert_eq!(direct.cached_input_price_per_million_usd, None);
        assert_eq!(direct.output_price_per_million_usd, None);

        let config = Config::new(MID_URL, MID_MODEL, Some("mid-key".to_string())).with_giga(
            GIGA_URL,
            GIGA_MODEL,
            Some("giga-key".to_string()),
        );
        assert_eq!(config.mid.input_price_per_million_usd, None);
        assert_eq!(config.mid.cached_input_price_per_million_usd, None);
        assert_eq!(config.mid.output_price_per_million_usd, None);

        let giga = config.giga.as_ref().expect("the giga block is recorded");
        assert_eq!(giga.input_price_per_million_usd, None);
        assert_eq!(giga.cached_input_price_per_million_usd, None);
        assert_eq!(giga.output_price_per_million_usd, None);

        let smoll = llm::factory::SmollSettings::new(SMOLL_URL, SMOLL_MODEL, "smoll-key")
            .expect("a non-blank key configures Smoll");
        assert_eq!(smoll.input_price_per_million_usd, None);
        assert_eq!(smoll.cached_input_price_per_million_usd, None);
        assert_eq!(smoll.output_price_per_million_usd, None);
    }

    /// The triage pass designs rather than runs: in hybrid mode it goes to the
    /// giga provider, and the phases that already had a route keep it.
    #[test]
    fn when_routing_triage_given_hybrid_mode_then_uses_giga_provider() {
        let config = giga_config().with_mode(Mode::Hybrid);

        assert_eq!(config.provider(Phase::Triage).name(), "responses");
        assert_eq!(config.provider_role(Phase::Triage), "giga");

        assert_eq!(config.provider(Phase::Plan).name(), "responses");
        assert_eq!(config.provider(Phase::Spec).name(), "responses");
        assert_eq!(config.provider(Phase::Tickets).name(), "responses");
        assert_eq!(config.provider(Phase::Review).name(), "responses");
        assert_eq!(config.provider_role(Phase::Plan), "giga");
        assert_eq!(config.provider_role(Phase::Review), "giga");
        assert_eq!(config.provider(Phase::Code).name(), "chat-completions");
        assert_eq!(config.provider_role(Phase::Code), "mid");
        assert_eq!(config.provider(Phase::Summary).name(), "anthropic-messages");
        assert_eq!(config.provider_role(Phase::Summary), "smoll");
    }

    /// The triage pass is served by the configured provider: mid in mid mode,
    /// giga in giga mode, and the other phases are untouched by the route.
    #[test]
    fn when_routing_triage_given_single_provider_modes_then_uses_configured_provider() {
        let mid = mid_config();
        assert_eq!(mid.provider(Phase::Triage).name(), "chat-completions");
        assert_eq!(mid.provider_role(Phase::Triage), "mid");
        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Code,
            Phase::Review,
        ] {
            assert_eq!(
                mid.provider(phase).name(),
                "chat-completions",
                "phase {phase:?}"
            );
            assert_eq!(mid.provider_role(phase), "mid", "phase {phase:?}");
        }
        assert_eq!(mid.provider(Phase::Summary).name(), "anthropic-messages");
        assert_eq!(mid.provider_role(Phase::Summary), "smoll");

        let giga = giga_config().with_mode(Mode::Giga);
        assert_eq!(giga.provider(Phase::Triage).name(), "responses");
        assert_eq!(giga.provider_role(Phase::Triage), "giga");
        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Code,
            Phase::Review,
        ] {
            assert_eq!(giga.provider(phase).name(), "responses", "phase {phase:?}");
            assert_eq!(giga.provider_role(phase), "giga", "phase {phase:?}");
        }
        assert_eq!(giga.provider(Phase::Summary).name(), "anthropic-messages");
        assert_eq!(giga.provider_role(Phase::Summary), "smoll");
    }

    /// Core entry-point behavior at the upper supported boundary: every
    /// canonical variable resolves into its own block in every mode, with the
    /// documented `max` effort, and every phase keeps its established route.
    #[test]
    fn when_configuring_given_every_canonical_environment_value_then_resolves_all_settings() {
        for mode in [Mode::Mid, Mode::Giga, Mode::Hybrid] {
            let values = canonical_environment(mode.label(), MAX_EFFORT, MAX_EFFORT);
            let config = Config::from_environment(&environment(&values))
                .unwrap_or_else(|message| panic!("{mode:?} environment must resolve: {message}"));

            assert_eq!(config.mode, mode);

            assert_eq!(config.mid.url, MID_URL);
            assert_eq!(config.mid.model, MID_MODEL);
            assert_eq!(config.mid.api_key.as_deref(), Some("sk-mid"));
            assert!(!config.mid.auth_disabled);
            assert_eq!(config.mid.reasoning_effort.as_deref(), Some(MAX_EFFORT));

            let giga = config
                .giga
                .as_ref()
                .expect("a complete environment always configures the giga block");
            assert_eq!(giga.url, GIGA_URL);
            assert_eq!(giga.model, GIGA_MODEL);
            assert_eq!(giga.api_key.as_deref(), Some("sk-giga"));
            assert!(giga.auth_disabled);
            assert_eq!(giga.reasoning_effort.as_deref(), Some(MAX_EFFORT));

            assert_routes(&config);
        }
    }

    /// Resolved Smoll coordinates -- endpoint, model and required key -- are
    /// exposed through the public Summary request and description in every
    /// mode, including the documented default endpoint when the URL is absent
    /// or blank.
    #[test]
    fn when_resolving_given_smoll_environment_then_exposes_values_through_summary_requests() {
        for mode in [Mode::Mid, Mode::Giga, Mode::Hybrid] {
            for url in [Some(SMOLL_URL), None, Some("   ")] {
                let mut values = canonical_environment(mode.label(), "high", "high");
                values.retain(|(name, _)| *name != "LLM_SMOLL_URL");
                if let Some(url) = url {
                    values.push(("LLM_SMOLL_URL", url));
                }

                let config =
                    Config::from_environment(&environment(&values)).unwrap_or_else(|message| {
                        panic!("{mode:?} environment must resolve: {message}")
                    });

                let expected_url = url
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(crate::llm::factory::DEFAULT_SMOLL_URL);
                assert_summary_settings(&config, expected_url, SMOLL_MODEL, "sk-smoll");
            }
        }
    }

    /// Upper supported boundary: `max` is preserved verbatim in both blocks in
    /// every mode, no sibling value changes, and each protocol's request carries
    /// it. No extra size limit is checked or introduced.
    #[test]
    fn when_configuring_given_max_reasoning_effort_then_preserves_mid_and_giga_values() {
        for mode in [Mode::Mid, Mode::Giga, Mode::Hybrid] {
            let maximum = Config::from_environment(&environment(&canonical_environment(
                mode.label(),
                MAX_EFFORT,
                MAX_EFFORT,
            )))
            .expect("the complete environment resolves");
            let lower = Config::from_environment(&environment(&canonical_environment(
                mode.label(),
                "low",
                "low",
            )))
            .expect("the complete environment resolves");

            assert_eq!(maximum.mode, lower.mode);

            assert_eq!(maximum.mid.url, lower.mid.url);
            assert_eq!(maximum.mid.model, lower.mid.model);
            assert_eq!(maximum.mid.api_key, lower.mid.api_key);
            assert_eq!(maximum.mid.auth_disabled, lower.mid.auth_disabled);
            assert_eq!(maximum.mid.reasoning_effort.as_deref(), Some(MAX_EFFORT));

            assert_summary_settings(&maximum, SMOLL_URL, SMOLL_MODEL, "sk-smoll");
            assert_summary_settings(&lower, SMOLL_URL, SMOLL_MODEL, "sk-smoll");

            let max_giga = maximum.giga.as_ref().expect("the giga block resolves");
            let low_giga = lower.giga.as_ref().expect("the giga block resolves");
            assert_eq!(max_giga.url, low_giga.url);
            assert_eq!(max_giga.model, low_giga.model);
            assert_eq!(max_giga.api_key, low_giga.api_key);
            assert_eq!(max_giga.auth_disabled, low_giga.auth_disabled);
            assert_eq!(max_giga.reasoning_effort.as_deref(), Some(MAX_EFFORT));

            assert_routes(&maximum);

            for phase in PHASES {
                let provider = maximum.provider(phase);
                let prepared = provider.prepare(&[Message::user("hi")], phase);
                if phase == Phase::Summary {
                    assert_eq!(
                        prepared.outgoing.body["model"],
                        json!(SMOLL_MODEL),
                        "the Smoll answer is unaffected by mid/giga effort"
                    );
                    continue;
                }
                let sent = match provider.name() {
                    "chat-completions" => prepared.outgoing.body["reasoning_effort"].clone(),
                    _ => prepared.outgoing.body["reasoning"]["effort"].clone(),
                };
                assert_eq!(sent, json!(MAX_EFFORT), "phase {phase:?}");
            }
        }
    }

    /// Edge-case table: in every mode the required Smoll model and key are each
    /// refused when absent, empty or whitespace-only, and the diagnostic names
    /// the missing canonical variable with configuration guidance.
    #[test]
    fn when_resolving_given_missing_empty_or_whitespace_smoll_coordinates_then_names_the_missing_variable()
     {
        for mode in [Mode::Mid, Mode::Giga, Mode::Hybrid] {
            for missing in ["LLM_SMOLL_MODEL", "LLM_SMOLL_API_KEY"] {
                for blank in [None, Some(""), Some("   "), Some("\t \n")] {
                    let mut values = canonical_environment(mode.label(), "high", "high");
                    values.retain(|(name, _)| {
                        *name != "LLM_SMOLL_MODEL" && *name != "LLM_SMOLL_API_KEY"
                    });
                    if missing == "LLM_SMOLL_MODEL" {
                        values.push(("LLM_SMOLL_API_KEY", "sk-smoll"));
                    } else {
                        values.push(("LLM_SMOLL_MODEL", SMOLL_MODEL));
                    }
                    if let Some(value) = blank {
                        values.push((missing, value));
                    }

                    let error = resolution_error(&values);
                    assert!(error.contains(missing), "mode {mode:?}: {error}");
                    assert!(error.contains("LLM_SMOLL_MODEL"), "mode {mode:?}: {error}");
                    assert!(
                        error.contains("LLM_SMOLL_API_KEY"),
                        "mode {mode:?}: {error}"
                    );
                    assert!(error.contains("Set"), "mode {mode:?}: {error}");
                }
            }
        }
    }

    /// Regression: an unknown mode and a half-written giga block keep their
    /// exact diagnostics in every mode when valid Smoll values are supplied.
    #[test]
    fn when_resolving_given_existing_giga_half_configuration_or_unknown_mode_then_preserves_existing_diagnostics()
     {
        let mut values = smoll_environment();
        values.push(("AGENT_MODE", "mystery"));
        assert_eq!(
            resolution_error(&values),
            "AGENT_MODE=\"mystery\" is not recognised; use mid, giga or hybrid."
        );

        let mut values = smoll_environment();
        values.push(("AGENT_MODE", "mid"));
        values.push(("LLM_GIGA_URL", GIGA_URL));
        assert_eq!(
            resolution_error(&values),
            "The giga provider is half-configured: LLM_GIGA_MODEL is not set. Set it, or unset the other one."
        );

        let mut values = smoll_environment();
        values.push(("AGENT_MODE", "mid"));
        values.push(("LLM_GIGA_MODEL", GIGA_MODEL));
        assert_eq!(
            resolution_error(&values),
            "The giga provider is half-configured: LLM_GIGA_URL is not set. Set it, or unset the other one."
        );

        for mode in [Mode::Giga, Mode::Hybrid] {
            let mut values = smoll_environment();
            values.push(("AGENT_MODE", mode.label()));
            let error = resolution_error(&values);
            assert!(error.contains("LLM_GIGA_URL"), "{mode:?}: {error}");
            assert!(error.contains("LLM_GIGA_MODEL"), "{mode:?}: {error}");
        }
    }

    /// Unauthorized/forbidden boundary: every Mid and Giga credential -- model,
    /// key, auth switch and endpoint -- plus the old generic and architect
    /// variables cannot satisfy a missing Smoll model or key; resolution never
    /// falls back to another provider.
    #[test]
    fn when_configuring_given_other_provider_credentials_without_smoll_credentials_then_refuses_provider_fallback()
     {
        for mode in [Mode::Mid, Mode::Giga, Mode::Hybrid] {
            let mut values = canonical_environment(mode.label(), "high", "high");
            values.retain(|(name, _)| !name.starts_with("LLM_SMOLL_"));
            values.extend([
                (
                    "LLM_PROVIDER_URL",
                    "https://legacy.test/v1/chat/completions",
                ),
                ("LLM_MODEL", "legacy-model"),
                ("LLM_API_KEY", "legacy-key"),
                ("LLM_AUTH", "none"),
                ("LLM_ARCHITECT_URL", "https://legacy.test/v1/responses"),
                ("LLM_ARCHITECT_MODEL", "legacy-giga-model"),
                ("LLM_ARCHITECT_API_KEY", "legacy-key"),
                ("LLM_ARCHITECT_AUTH", "none"),
            ]);

            let error = resolution_error(&values);
            assert!(
                error.contains("LLM_SMOLL_MODEL") || error.contains("LLM_SMOLL_API_KEY"),
                "mode {mode:?}: {error}"
            );
            assert!(!error.contains("mid-model"), "mode {mode:?}: {error}");
            assert!(!error.contains("giga-model"), "mode {mode:?}: {error}");
        }
    }

    /// Minimum valid boundary: a complete Smoll block is enough for the Summary
    /// coordinates in every mode, subject only to the existing mode requirements.
    #[test]
    fn when_configuring_given_only_smoll_and_mode_requirements_then_resolves_the_summary_route() {
        for mode in [Mode::Mid, Mode::Giga, Mode::Hybrid] {
            let mut values = smoll_environment();
            values.push(("AGENT_MODE", mode.label()));
            if mode.needs_giga() {
                values.push(("LLM_GIGA_URL", GIGA_URL));
                values.push(("LLM_GIGA_MODEL", GIGA_MODEL));
            }

            let config = Config::from_environment(&environment(&values))
                .unwrap_or_else(|message| panic!("{mode:?} must resolve: {message}"));
            assert_summary_settings(&config, SMOLL_URL, SMOLL_MODEL, "sk-smoll");
        }
    }

    /// The phase-resolved model mirrors the provider route: Mid and Giga name
    /// their own configured model for every non-Summary phase, hybrid sends
    /// Code to the Mid model and its planning/audit phases to the Giga model,
    /// and Summary always names the dedicated Smoll model.
    #[test]
    fn when_configuring_given_each_agent_mode_then_selects_the_phase_provider_model() {
        let configurations = [
            (Mode::Mid, mid_config()),
            (Mode::Giga, giga_config().with_mode(Mode::Giga)),
            (Mode::Hybrid, giga_config().with_mode(Mode::Hybrid)),
        ];

        for (mode, config) in configurations {
            for phase in PHASES {
                let expected = match (mode, phase) {
                    (_, Phase::Summary) => SMOLL_MODEL,
                    (Mode::Mid, _) => MID_MODEL,
                    (Mode::Giga, _) => GIGA_MODEL,
                    (Mode::Hybrid, Phase::Code) => MID_MODEL,
                    (Mode::Hybrid, _) => GIGA_MODEL,
                };
                assert_eq!(
                    config.provider_model(phase),
                    expected,
                    "mode {mode:?} phase {phase:?}"
                );
                assert_eq!(
                    config
                        .provider(phase)
                        .prepare(&[Message::user("hi")], phase)
                        .outgoing
                        .body["model"],
                    json!(expected),
                    "mode {mode:?} phase {phase:?} wire model"
                );
            }
        }
    }

    /// One provider's three optional per-million-token rates, as the fixtures
    /// and expectations below express them: a whole role at a glance, so a test
    /// can compare the serving provider's triple against a sibling's.
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Rates {
        input: Option<f64>,
        cached_input: Option<f64>,
        output: Option<f64>,
    }

    impl Rates {
        /// Nothing configured: the empty/nil end of the rate table.
        const NONE: Rates = Rates {
            input: None,
            cached_input: None,
            output: None,
        };

        /// A provider priced for input tokens only, the shape every earlier
        /// configuration and every example in `.env.example` before this ticket
        /// has.
        const fn input_only(input: Option<f64>) -> Rates {
            Rates {
                input,
                cached_input: None,
                output: None,
            }
        }

        /// The three rates in the order the fixture writes them to the
        /// environment: input, cached input, output.
        const fn triple(self) -> [Option<f64>; 3] {
            [self.input, self.cached_input, self.output]
        }
    }

    /// One rate kind, so a table can step over input, cached input and output
    /// without repeating the accessor call three times.
    #[derive(Clone, Copy, Debug)]
    enum RateKind {
        Input,
        CachedInput,
        Output,
    }

    impl RateKind {
        const ALL: [RateKind; 3] = [RateKind::Input, RateKind::CachedInput, RateKind::Output];

        /// This kind of rate from one provider's fixture triple.
        fn of(self, rates: Rates) -> Option<f64> {
            match self {
                RateKind::Input => rates.input,
                RateKind::CachedInput => rates.cached_input,
                RateKind::Output => rates.output,
            }
        }

        /// This kind of rate for `phase`, read only through the public
        /// accessors.
        fn select(self, config: &Config, phase: Phase) -> Option<f64> {
            match self {
                RateKind::Input => config.input_price(phase),
                RateKind::CachedInput => config.cached_input_price(phase),
                RateKind::Output => config.output_price(phase),
            }
        }
    }

    /// The nine canonical rate variables, one triple per provider role in the
    /// same order as [`Rates::triple`].
    const RATE_VARIABLES: [(&str, [&str; 3]); 3] = [
        (
            "mid",
            [
                "LLM_MID_INPUT_PRICE_PER_MILLION_USD",
                "LLM_MID_CACHED_INPUT_PRICE_PER_MILLION_USD",
                "LLM_MID_OUTPUT_PRICE_PER_MILLION_USD",
            ],
        ),
        (
            "giga",
            [
                "LLM_GIGA_INPUT_PRICE_PER_MILLION_USD",
                "LLM_GIGA_CACHED_INPUT_PRICE_PER_MILLION_USD",
                "LLM_GIGA_OUTPUT_PRICE_PER_MILLION_USD",
            ],
        ),
        (
            "smoll",
            [
                "LLM_SMOLL_INPUT_PRICE_PER_MILLION_USD",
                "LLM_SMOLL_CACHED_INPUT_PRICE_PER_MILLION_USD",
                "LLM_SMOLL_OUTPUT_PRICE_PER_MILLION_USD",
            ],
        ),
    ];

    /// Each fixture role's three rates, distinct from the other roles' so a
    /// phase's price identifies the provider that served it: mid 1.25 / 0.31 /
    /// 5.0, giga 2.5 / 0.62 / 10.0, smoll 3.75 / 0.94 / 15.0.
    const MID_RATES: Rates = Rates {
        input: Some(1.25),
        cached_input: Some(0.31),
        output: Some(5.0),
    };
    const GIGA_RATES: Rates = Rates {
        input: Some(2.5),
        cached_input: Some(0.62),
        output: Some(10.0),
    };
    const SMOLL_RATES: Rates = Rates {
        input: Some(3.75),
        cached_input: Some(0.94),
        output: Some(15.0),
    };

    /// The fixture triple of one canonical provider role, or a panic naming the
    /// role that is not one.
    fn role_rates(role: &str) -> Rates {
        match role {
            "mid" => MID_RATES,
            "giga" => GIGA_RATES,
            "smoll" => SMOLL_RATES,
            other => panic!("`{other}` is not a provider role"),
        }
    }

    /// Every phase price of one configuration, read only through the public
    /// accessors.
    fn phase_rates(config: &Config, phase: Phase) -> Rates {
        Rates {
            input: config.input_price(phase),
            cached_input: config.cached_input_price(phase),
            output: config.output_price(phase),
        }
    }

    /// Build a configuration whose three provider blocks each carry the given
    /// optional rates, through the same isolated environment seam production
    /// uses. The phase-routed price tests can distinguish the serving
    /// provider's value from a sibling provider's value without mutating
    /// private settings.
    fn fully_priced_config(mode: Mode, mid: Rates, giga: Rates, smoll: Rates) -> Config {
        let mut values: Vec<(&'static str, String)> =
            canonical_environment(mode.label(), "high", "high")
                .into_iter()
                .map(|(name, value)| (name, value.to_string()))
                .collect();

        // The shared canonical table disables giga auth for diagnostic cases;
        // these routing fixtures keep a keyed giga credential so the existing
        // authentication invariants stay covered.
        if let Some((_, auth)) = values.iter_mut().find(|(name, _)| *name == "LLM_GIGA_AUTH") {
            *auth = "bearer".to_string();
        }

        for ((_, variables), rates) in RATE_VARIABLES.iter().zip([mid, giga, smoll]) {
            for (variable, rate) in variables.iter().zip(rates.triple()) {
                if let Some(rate) = rate {
                    values.push((*variable, rate.to_string()));
                }
            }
        }

        let environment = |name: &str| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        };
        Config::from_environment(&environment)
            .unwrap_or_else(|message| panic!("{mode:?} priced environment must resolve: {message}"))
    }

    /// The same fixture restricted to the input rate every earlier test
    /// configures, so those tests keep saying only what they mean.
    fn priced_config(
        mode: Mode,
        mid_rate: Option<f64>,
        giga_rate: Option<f64>,
        smoll_rate: Option<f64>,
    ) -> Config {
        fully_priced_config(
            mode,
            Rates::input_only(mid_rate),
            Rates::input_only(giga_rate),
            Rates::input_only(smoll_rate),
        )
    }

    /// Core entry-point behavior: every phase selects the optional input rate of
    /// the provider that actually serves it -- Mid's rate in Mid mode, Giga's in
    /// Giga mode, and, in Hybrid mode, Mid for Code and Giga for the planning and
    /// audit phases -- while Summary selects the dedicated Smoll rate in every
    /// mode. The examples use 1.25, 2.5 and 3.75.
    #[test]
    fn when_routing_each_phase_given_mode_and_provider_rates_then_selects_the_serving_providers_rate()
     {
        let configurations = [
            (
                Mode::Mid,
                priced_config(Mode::Mid, Some(1.25), Some(2.5), Some(3.75)),
            ),
            (
                Mode::Giga,
                priced_config(Mode::Giga, Some(1.25), Some(2.5), Some(3.75)),
            ),
            (
                Mode::Hybrid,
                priced_config(Mode::Hybrid, Some(1.25), Some(2.5), Some(3.75)),
            ),
        ];

        for (mode, config) in configurations {
            for phase in PHASES {
                let expected = match (mode, phase) {
                    (_, Phase::Summary) => Some(3.75),
                    (Mode::Mid, _) => Some(1.25),
                    (Mode::Giga, _) => Some(2.5),
                    (Mode::Hybrid, Phase::Code) => Some(1.25),
                    (Mode::Hybrid, _) => Some(2.5),
                };
                assert_eq!(
                    config.input_price(phase),
                    expected,
                    "mode {mode:?} phase {phase:?}"
                );
                assert_ne!(
                    config.provider_role(phase),
                    "hybrid",
                    "the mode's own name is never a rate role: {phase:?}"
                );
            }
        }

        let hybrid = priced_config(Mode::Hybrid, Some(1.25), Some(2.5), Some(3.75));
        assert_eq!(hybrid.input_price(Phase::Code), Some(1.25));
        assert_eq!(hybrid.input_price(Phase::Plan), Some(2.5));
        assert_eq!(hybrid.input_price(Phase::Summary), Some(3.75));
    }

    /// Edge-case table: a missing rate belongs only to the selected provider.
    /// Another provider's configured rate is never inherited, and Summary most
    /// of all does not fall back to Mid or Giga.
    #[test]
    fn when_routing_a_phase_given_selected_rate_is_missing_then_returns_none_without_inheriting_another_rate()
     {
        let mid_missing = priced_config(Mode::Mid, None, Some(2.5), Some(3.75));
        assert_eq!(mid_missing.input_price(Phase::Plan), None);
        assert_eq!(mid_missing.input_price(Phase::Code), None);
        assert_eq!(mid_missing.input_price(Phase::Summary), Some(3.75));

        let giga_missing = priced_config(Mode::Hybrid, Some(1.25), None, Some(3.75));
        assert_eq!(giga_missing.input_price(Phase::Plan), None);
        assert_eq!(giga_missing.input_price(Phase::Spec), None);
        assert_eq!(giga_missing.input_price(Phase::Tickets), None);
        assert_eq!(giga_missing.input_price(Phase::Triage), None);
        assert_eq!(giga_missing.input_price(Phase::Review), None);
        assert_eq!(giga_missing.input_price(Phase::Code), Some(1.25));
        assert_eq!(giga_missing.input_price(Phase::Summary), Some(3.75));

        let summary_missing = priced_config(Mode::Giga, Some(1.25), Some(2.5), None);
        assert_eq!(summary_missing.input_price(Phase::Summary), None);
        assert_eq!(summary_missing.input_price(Phase::Plan), Some(2.5));
    }

    /// Minimum valid boundary: zero is a configured rate, so the selected route
    /// keeps `Some(0.0)` and is not mistaken for a missing price.
    #[test]
    fn when_routing_a_phase_given_selected_provider_has_zero_rate_then_preserves_some_zero() {
        for mode in [Mode::Mid, Mode::Giga, Mode::Hybrid] {
            let config = priced_config(mode, Some(0.0), Some(0.0), Some(0.0));
            for phase in PHASES {
                assert_eq!(
                    config.input_price(phase),
                    Some(0.0),
                    "mode {mode:?} phase {phase:?}"
                );
            }
        }

        let hybrid_mid_zero = priced_config(Mode::Hybrid, Some(0.0), Some(2.5), Some(3.75));
        assert_eq!(hybrid_mid_zero.input_price(Phase::Code), Some(0.0));
        assert_eq!(hybrid_mid_zero.input_price(Phase::Plan), Some(2.5));
    }

    /// Upper boundary: a finite configured rate is returned unchanged by phase
    /// selection; routing never rounds, scales or clamps it.
    #[test]
    fn when_routing_phases_given_finite_upper_rates_then_returns_the_selected_provider_rate() {
        let maximum = f64::MAX;
        for mode in [Mode::Mid, Mode::Giga, Mode::Hybrid] {
            let config = priced_config(mode, Some(maximum), Some(maximum), Some(maximum));
            for phase in PHASES {
                assert_eq!(
                    config.input_price(phase),
                    Some(maximum),
                    "mode {mode:?} phase {phase:?}"
                );
            }
        }

        let distinct = priced_config(
            Mode::Hybrid,
            Some(f64::MAX / 2.0),
            Some(f64::MAX),
            Some(f64::MAX / 4.0),
        );
        assert_eq!(distinct.input_price(Phase::Code), Some(f64::MAX / 2.0));
        assert_eq!(distinct.input_price(Phase::Review), Some(f64::MAX));
        assert_eq!(distinct.input_price(Phase::Summary), Some(f64::MAX / 4.0));
    }

    /// Regression: optional prices do not change which provider or role serves
    /// any phase in any existing mode.
    #[test]
    fn when_routing_phases_given_existing_provider_modes_then_keeps_provider_and_role_routes_unchanged()
     {
        for mode in [Mode::Mid, Mode::Giga, Mode::Hybrid] {
            let config = priced_config(mode, Some(1.25), Some(2.5), Some(3.75));
            assert_routes(&config);
            for phase in PHASES {
                let (provider, role) = expected_route(mode, phase);
                assert_eq!(
                    config.provider(phase).name(),
                    provider,
                    "mode {mode:?} phase {phase:?}"
                );
                assert_eq!(
                    config.provider_role(phase),
                    role,
                    "mode {mode:?} phase {phase:?}"
                );
                assert_ne!(
                    config.provider_role(phase),
                    "hybrid",
                    "mode {mode:?} phase {phase:?}"
                );
            }
        }
    }

    /// Malformed/invalid payload: an invalid selected rate fails configuration
    /// resolution with the diagnostic naming the exact canonical variable; it
    /// never becomes a route fallback.
    #[test]
    fn when_building_configuration_given_invalid_selected_rate_then_preserves_the_named_configuration_error()
     {
        for mode in [Mode::Mid, Mode::Giga, Mode::Hybrid] {
            for variable in [
                "LLM_MID_INPUT_PRICE_PER_MILLION_USD",
                "LLM_GIGA_INPUT_PRICE_PER_MILLION_USD",
                "LLM_SMOLL_INPUT_PRICE_PER_MILLION_USD",
            ] {
                let mut values = canonical_environment(mode.label(), "high", "high");
                values.push((variable, "not-a-rate"));
                assert_eq!(
                    resolution_error(&values),
                    format!(
                        "{variable} must be a finite, non-negative number of USD per million input tokens."
                    ),
                    "mode {mode:?} variable {variable}"
                );
            }
        }
    }

    /// Unauthorized/forbidden boundary: selecting a rate adds no authorization
    /// path -- every phase still sends the credential its existing provider
    /// route already selected.
    #[test]
    fn when_routing_phases_given_configured_credentials_then_keeps_existing_provider_authentication()
     {
        for mode in [Mode::Mid, Mode::Giga, Mode::Hybrid] {
            let config = priced_config(mode, Some(1.25), Some(2.5), Some(3.75));
            for phase in PHASES {
                let prepared = config
                    .provider(phase)
                    .prepare(&[Message::user("hi")], phase);
                let expected = match (mode, phase) {
                    (_, Phase::Summary) => Auth::Anthropic("sk-smoll".to_string()),
                    (Mode::Mid, _) | (Mode::Hybrid, Phase::Code) => {
                        Auth::Bearer("sk-mid".to_string())
                    }
                    _ => Auth::Bearer("sk-giga".to_string()),
                };
                assert_eq!(
                    prepared.outgoing.auth, expected,
                    "mode {mode:?} phase {phase:?}"
                );
            }
        }
    }

    /// Regression: existing configurations that omit every optional price keep
    /// resolving and leave each provider's rate unset.
    #[test]
    fn when_resolving_configuration_given_all_price_variables_omitted_then_keeps_existing_startup_behavior()
     {
        for mode in [Mode::Mid, Mode::Giga, Mode::Hybrid] {
            let values = canonical_environment(mode.label(), "high", "high");
            assert!(
                values
                    .iter()
                    .all(|(name, _)| !name.ends_with("_INPUT_PRICE_PER_MILLION_USD")),
                "the regression environment must omit every price"
            );

            let config = Config::from_environment(&environment(&values))
                .unwrap_or_else(|message| panic!("{mode:?} must resolve: {message}"));

            for phase in PHASES {
                assert_eq!(
                    config.input_price(phase),
                    None,
                    "mode {mode:?} phase {phase:?}"
                );
            }
            assert_routes(&config);
        }
    }

    /// Unauthorized/forbidden boundary: prices are local metadata. Adding them
    /// leaves every credential, endpoint, authentication scheme and request body
    /// exactly as the unpriced configuration produced.
    #[test]
    fn when_configuring_rates_given_existing_credentials_then_preserves_authentication_and_request_payload()
     {
        for mode in [Mode::Mid, Mode::Giga, Mode::Hybrid] {
            let unpriced = canonical_environment(mode.label(), "high", "high");
            let mut priced = unpriced.clone();
            priced.extend([
                ("LLM_MID_INPUT_PRICE_PER_MILLION_USD", "1.25"),
                ("LLM_GIGA_INPUT_PRICE_PER_MILLION_USD", "2.5"),
                ("LLM_SMOLL_INPUT_PRICE_PER_MILLION_USD", "3.75"),
            ]);

            let baseline = Config::from_environment(&environment(&unpriced))
                .unwrap_or_else(|message| panic!("{mode:?} must resolve: {message}"));
            let configured = Config::from_environment(&environment(&priced))
                .unwrap_or_else(|message| panic!("{mode:?} must resolve: {message}"));

            for phase in PHASES {
                assert_eq!(baseline.input_price(phase), None, "phase {phase:?}");
                let expected_rate = match (mode, phase) {
                    (_, Phase::Summary) => 3.75,
                    (Mode::Mid, _) => 1.25,
                    (Mode::Giga, _) => 2.5,
                    (Mode::Hybrid, Phase::Code) => 1.25,
                    (Mode::Hybrid, _) => 2.5,
                };
                assert_eq!(
                    configured.input_price(phase),
                    Some(expected_rate),
                    "phase {phase:?}"
                );

                let sent = configured
                    .provider(phase)
                    .prepare(&[Message::user("hi")], phase);
                let original = baseline
                    .provider(phase)
                    .prepare(&[Message::user("hi")], phase);

                let expected_auth = match (mode, phase) {
                    (_, Phase::Summary) => Auth::Anthropic("sk-smoll".to_string()),
                    (Mode::Mid, _) | (Mode::Hybrid, Phase::Code) => {
                        Auth::Bearer("sk-mid".to_string())
                    }
                    _ => Auth::None,
                };
                assert_eq!(sent.outgoing.auth, expected_auth, "phase {phase:?}");
                assert_eq!(sent.outgoing.url, original.outgoing.url, "phase {phase:?}");
                assert_eq!(
                    sent.outgoing.auth, original.outgoing.auth,
                    "phase {phase:?}"
                );
                assert_eq!(
                    sent.outgoing.body, original.outgoing.body,
                    "phase {phase:?}"
                );
                assert!(
                    !sent.outgoing.body.to_string().contains("INPUT_PRICE"),
                    "phase {phase:?}: pricing leaked onto the wire"
                );
            }
        }
    }

    /// Core entry-point behavior (minimum valid boundary): a hybrid
    /// configuration with a configured giga block derives a retry whose Code
    /// route is the responses provider with the giga role, model and configured
    /// rate, while the original configuration still runs Code on mid.
    #[test]
    fn when_deriving_code_retry_given_hybrid_with_giga_then_selects_giga_and_preserves_original_route()
     {
        let hybrid = priced_config(Mode::Hybrid, Some(1.25), Some(2.5), Some(3.75));

        let retry = hybrid.code_retry_config();

        assert_phase_route(
            &retry,
            Phase::Code,
            "responses",
            "giga",
            GIGA_MODEL,
            Some(2.5),
        );
        assert_phase_route(
            &hybrid,
            Phase::Code,
            "chat-completions",
            "mid",
            MID_MODEL,
            Some(1.25),
        );
        assert_eq!(
            retry
                .provider(Phase::Code)
                .prepare(&[Message::user("hi")], Phase::Code)
                .outgoing
                .url,
            GIGA_URL
        );
        assert_eq!(
            hybrid
                .provider(Phase::Code)
                .prepare(&[Message::user("hi")], Phase::Code)
                .outgoing
                .url,
            MID_URL
        );
    }

    /// Edge-case table (empty/nil and minimum valid boundaries): Mid, Giga and a
    /// hybrid configuration without a giga block keep their own Code route, and
    /// the absent giga provider is never selected.
    #[test]
    fn when_deriving_code_retry_given_mid_giga_or_hybrid_without_giga_then_preserves_mode_specific_route()
     {
        let mid = priced_config(Mode::Mid, Some(1.25), Some(2.5), Some(3.75));
        let giga = priced_config(Mode::Giga, Some(1.25), Some(2.5), Some(3.75));
        let unpriced_mid = Config::new(MID_URL, MID_MODEL, None);
        let hybrid_without_giga = mid_config().with_mode(Mode::Hybrid);

        let cases = [
            (
                "mid",
                &mid,
                "chat-completions",
                "mid",
                MID_MODEL,
                Some(1.25),
            ),
            ("giga", &giga, "responses", "giga", GIGA_MODEL, Some(2.5)),
            (
                "unpriced mid",
                &unpriced_mid,
                "chat-completions",
                "mid",
                MID_MODEL,
                None,
            ),
            (
                "hybrid without giga",
                &hybrid_without_giga,
                "chat-completions",
                "mid",
                MID_MODEL,
                None,
            ),
        ];

        for (_case, config, protocol, role, model, price) in cases {
            let retry = config.code_retry_config();
            assert_phase_route(&retry, Phase::Code, protocol, role, model, price);
            assert_phase_route(config, Phase::Code, protocol, role, model, price);
        }

        // The two configurations that are not hybrid keep their mode, and the
        // hybrid configuration without a giga block never escalates.
        assert_eq!(mid.code_retry_config().describe(), mid.describe());
        assert_eq!(giga.code_retry_config().describe(), giga.describe());
        assert_eq!(
            hybrid_without_giga
                .code_retry_config()
                .provider(Phase::Code)
                .name(),
            "chat-completions"
        );
        let retry_summary = hybrid_without_giga
            .code_retry_config()
            .provider(Phase::Summary);
        assert_eq!(retry_summary.name(), "anthropic-messages");
        let prepared = retry_summary.prepare(&[Message::user("hi")], Phase::Summary);
        assert_eq!(prepared.outgoing.url, SMOLL_URL);
        assert_eq!(prepared.outgoing.body["model"], json!(SMOLL_MODEL));
        assert_eq!(
            prepared.outgoing.auth,
            Auth::Anthropic("smoll-key".to_string())
        );
    }

    /// Contract and invariant check: deriving a retry changes only the mode --
    /// the giga block's credentials, reasoning settings, prices and Summary's
    /// Smoll provider, model and rate survive unchanged, and both configurations
    /// still send through the transport the caller installed.
    #[test]
    fn when_deriving_code_retry_given_configured_provider_blocks_then_changes_only_mode_and_preserves_summary_route()
     {
        let recording = Arc::new(RecordingTransport::default());
        let hybrid = priced_config(Mode::Hybrid, Some(1.25), Some(2.5), Some(3.75))
            .with_transport(recording.clone());

        let retry = hybrid.code_retry_config();

        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Triage,
            Phase::Review,
        ] {
            assert_phase_route(&retry, phase, "responses", "giga", GIGA_MODEL, Some(2.5));
            let derived = retry
                .provider(phase)
                .prepare(&[Message::user("hi")], phase)
                .outgoing;
            let original = hybrid
                .provider(phase)
                .prepare(&[Message::user("hi")], phase)
                .outgoing;
            assert_eq!(derived.url, GIGA_URL, "phase {phase:?} endpoint");
            assert_eq!(
                derived.auth,
                Auth::Bearer("sk-giga".to_string()),
                "phase {phase:?} credential"
            );
            assert_eq!(
                derived.body["reasoning"],
                json!({ "effort": "high" }),
                "phase {phase:?} reasoning"
            );
            assert_eq!(derived.body, original.body, "phase {phase:?} payload");
        }

        // The mid block derivation started from keeps its own credential, model
        // and reasoning setting for the original configuration's Code route.
        let original_code = hybrid
            .provider(Phase::Code)
            .prepare(&[Message::user("hi")], Phase::Code)
            .outgoing;
        assert_eq!(original_code.url, MID_URL);
        assert_eq!(original_code.auth, Auth::Bearer("sk-mid".to_string()));
        assert_eq!(original_code.body["model"], json!(MID_MODEL));
        assert_eq!(original_code.body["reasoning_effort"], json!("high"));
        assert_eq!(hybrid.input_price(Phase::Code), Some(1.25));

        let retry_code = retry
            .provider(Phase::Code)
            .prepare(&[Message::user("hi")], Phase::Code)
            .outgoing;
        assert_eq!(retry_code.url, GIGA_URL);
        assert_eq!(retry_code.auth, Auth::Bearer("sk-giga".to_string()));
        assert_eq!(retry_code.body["model"], json!(GIGA_MODEL));
        assert_eq!(retry_code.body["reasoning"], json!({ "effort": "high" }));
        assert_eq!(retry.input_price(Phase::Code), Some(2.5));

        // Summary stays on the configured Smoll Anthropic Messages provider on
        // both the original and the derived configuration.
        for config in [&hybrid, &retry] {
            assert_summary_settings(config, SMOLL_URL, SMOLL_MODEL, "sk-smoll");
            assert_eq!(config.input_price(Phase::Summary), Some(3.75));
        }

        hybrid
            .send(&original_code)
            .unwrap_or_else(|error| panic!("the original configuration must send: {error}"));
        retry
            .send(&retry_code)
            .unwrap_or_else(|error| panic!("the derived configuration must send: {error}"));
        let sent = recording
            .sent
            .lock()
            .expect("the recording transport is never poisoned");
        assert_eq!(sent.len(), 2, "both configurations share one transport");
        assert_eq!(sent[0].0, MID_URL);
        assert_eq!(sent[1].0, GIGA_URL);
    }

    /// Edge-case table (empty/nil): a hybrid configuration with an unpriced giga
    /// block derives a giga Code route that stays unpriced instead of inheriting
    /// the mid rate.
    #[test]
    fn when_deriving_code_retry_given_unpriced_giga_then_does_not_inherit_mid_price() {
        let hybrid = priced_config(Mode::Hybrid, Some(1.25), None, Some(3.75));

        let retry = hybrid.code_retry_config();

        assert_phase_route(&retry, Phase::Code, "responses", "giga", GIGA_MODEL, None);
        assert_phase_route(
            &hybrid,
            Phase::Code,
            "chat-completions",
            "mid",
            MID_MODEL,
            Some(1.25),
        );
        assert_eq!(retry.input_price(Phase::Summary), Some(3.75));
    }

    /// Core entry-point behavior: cached-input and output prices follow exactly
    /// the routing the input price already uses -- Smoll for Summary in every
    /// mode, Mid in Mid mode, Giga in Giga mode, and in Hybrid mode Mid for Code
    /// against Giga for Plan, Spec, Tickets, Triage and Review. The three roles
    /// carry 1.25 / 0.31 / 5.0, 2.5 / 0.62 / 10.0 and 3.75 / 0.94 / 15.0, so a
    /// selected triple names the role that served the phase.
    #[test]
    fn when_selecting_phase_prices_given_each_provider_mode_then_returns_the_selected_roles_rates()
    {
        let configurations = [
            (
                Mode::Mid,
                fully_priced_config(Mode::Mid, MID_RATES, GIGA_RATES, SMOLL_RATES),
            ),
            (
                Mode::Giga,
                fully_priced_config(Mode::Giga, MID_RATES, GIGA_RATES, SMOLL_RATES),
            ),
            (
                Mode::Hybrid,
                fully_priced_config(Mode::Hybrid, MID_RATES, GIGA_RATES, SMOLL_RATES),
            ),
        ];

        for (mode, config) in configurations {
            for phase in PHASES {
                let expected = match (mode, phase) {
                    (_, Phase::Summary) => SMOLL_RATES,
                    (Mode::Mid, _) => MID_RATES,
                    (Mode::Giga, _) => GIGA_RATES,
                    (Mode::Hybrid, Phase::Code) => MID_RATES,
                    (Mode::Hybrid, _) => GIGA_RATES,
                };
                assert_eq!(
                    phase_rates(&config, phase),
                    expected,
                    "mode {mode:?} phase {phase:?}"
                );
                for kind in RateKind::ALL {
                    assert_eq!(
                        kind.select(&config, phase),
                        kind.of(expected),
                        "mode {mode:?} phase {phase:?} {kind:?}"
                    );
                }
            }
        }

        // The hybrid split read straight off the accessors: Code is the Mid
        // provider's rate and every planning or audit phase is Giga's.
        let hybrid = fully_priced_config(Mode::Hybrid, MID_RATES, GIGA_RATES, SMOLL_RATES);
        assert_eq!(hybrid.cached_input_price(Phase::Code), Some(0.31));
        assert_eq!(hybrid.output_price(Phase::Code), Some(5.0));
        assert_eq!(hybrid.cached_input_price(Phase::Plan), Some(0.62));
        assert_eq!(hybrid.output_price(Phase::Review), Some(10.0));
        assert_eq!(hybrid.cached_input_price(Phase::Summary), Some(0.94));
        assert_eq!(hybrid.output_price(Phase::Summary), Some(15.0));
    }

    /// Edge-case table: when the selected role has no cached-input or output
    /// rate, that rate stays `None`. The other providers' configured rates are
    /// never substituted, whichever role is the unpriced one and whichever rate
    /// kind is missing.
    #[test]
    fn when_selecting_phase_prices_given_the_selected_role_has_no_rate_then_does_not_fall_back() {
        let cases = [
            (
                "mid mode, unpriced mid",
                Mode::Mid,
                "mid",
                fully_priced_config(Mode::Mid, Rates::NONE, GIGA_RATES, SMOLL_RATES),
            ),
            (
                "giga mode, unpriced giga",
                Mode::Giga,
                "giga",
                fully_priced_config(Mode::Giga, MID_RATES, Rates::NONE, SMOLL_RATES),
            ),
            (
                "hybrid, unpriced mid",
                Mode::Hybrid,
                "mid",
                fully_priced_config(Mode::Hybrid, Rates::NONE, GIGA_RATES, SMOLL_RATES),
            ),
            (
                "hybrid, unpriced giga",
                Mode::Hybrid,
                "giga",
                fully_priced_config(Mode::Hybrid, MID_RATES, Rates::NONE, SMOLL_RATES),
            ),
            (
                "hybrid, unpriced smoll",
                Mode::Hybrid,
                "smoll",
                fully_priced_config(Mode::Hybrid, MID_RATES, GIGA_RATES, Rates::NONE),
            ),
        ];

        for (label, mode, unpriced_role, config) in cases {
            for phase in PHASES {
                let (_, selected_role) = expected_route(mode, phase);
                for kind in RateKind::ALL {
                    let expected = if selected_role == unpriced_role {
                        None
                    } else {
                        kind.of(role_rates(selected_role))
                    };
                    assert_eq!(
                        kind.select(&config, phase),
                        expected,
                        "{label}: phase {phase:?} selected {selected_role} {kind:?}"
                    );
                }
            }
        }
    }

    /// Contract and invariant check: deriving the Hybrid Code retry keeps every
    /// configured rate on both provider blocks, so the retry's Code route
    /// selects Giga's rates while the configuration it derives from keeps Mid
    /// for Code and Summary keeps the Smoll provider and rate.
    #[test]
    fn when_retrying_hybrid_code_given_giga_rates_are_configured_then_retry_uses_giga_without_changing_original_routing()
     {
        let hybrid = fully_priced_config(Mode::Hybrid, MID_RATES, GIGA_RATES, SMOLL_RATES);

        let retry = hybrid.code_retry_config();

        assert_eq!(retry.provider_role(Phase::Code), "giga");
        assert_eq!(phase_rates(&retry, Phase::Code), GIGA_RATES);
        assert_eq!(retry.cached_input_price(Phase::Code), Some(0.62));
        assert_eq!(retry.output_price(Phase::Code), Some(10.0));
        assert_eq!(retry.provider_role(Phase::Summary), "smoll");
        assert_eq!(phase_rates(&retry, Phase::Summary), SMOLL_RATES);
        assert_eq!(retry.cached_input_price(Phase::Summary), Some(0.94));
        assert_eq!(retry.output_price(Phase::Summary), Some(15.0));

        assert_eq!(hybrid.provider_role(Phase::Code), "mid");
        assert_eq!(phase_rates(&hybrid, Phase::Code), MID_RATES);
        assert_eq!(hybrid.cached_input_price(Phase::Code), Some(0.31));
        assert_eq!(hybrid.output_price(Phase::Code), Some(5.0));
        assert_eq!(hybrid.provider_role(Phase::Summary), "smoll");
        assert_eq!(phase_rates(&hybrid, Phase::Summary), SMOLL_RATES);
        assert_eq!(phase_rates(&hybrid, Phase::Plan), GIGA_RATES);
    }

    /// Regression: configurations that configure only the input rate keep their
    /// existing input-price routing, and their cached-input and output prices
    /// stay unset rather than being derived from the input price.
    #[test]
    fn when_resolving_phase_prices_given_existing_input_rates_then_preserves_input_price_routing() {
        for mode in [Mode::Mid, Mode::Giga, Mode::Hybrid] {
            let config = priced_config(mode, Some(1.25), Some(2.5), Some(3.75));
            for phase in PHASES {
                let expected = match (mode, phase) {
                    (_, Phase::Summary) => Some(3.75),
                    (Mode::Mid, _) => Some(1.25),
                    (Mode::Giga, _) => Some(2.5),
                    (Mode::Hybrid, Phase::Code) => Some(1.25),
                    (Mode::Hybrid, _) => Some(2.5),
                };
                assert_eq!(
                    config.input_price(phase),
                    expected,
                    "mode {mode:?} phase {phase:?} input"
                );
                assert_eq!(
                    config.cached_input_price(phase),
                    None,
                    "mode {mode:?} phase {phase:?} cached input"
                );
                assert_eq!(
                    config.output_price(phase),
                    None,
                    "mode {mode:?} phase {phase:?} output"
                );
            }
        }

        let hybrid = priced_config(Mode::Hybrid, Some(1.25), Some(2.5), Some(3.75));
        assert_eq!(hybrid.input_price(Phase::Code), Some(1.25));
        assert_eq!(hybrid.input_price(Phase::Plan), Some(2.5));
        assert_eq!(hybrid.input_price(Phase::Summary), Some(3.75));
    }
}
