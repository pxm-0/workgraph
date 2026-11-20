//! Provider profiles (design §5.1, §7.1, D6). Built only from the owner's `factory.toml`, never
//! from factory or LLM output. A profile maps tiers to its own model ids.

use serde::Deserialize;
use std::collections::BTreeMap;
use wg_factory::api::Tier;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Subscription,
    AnthropicApi,
    Gateway { base_url: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Provider {
    pub id: String,
    pub kind: Kind,
    /// The keychain item `wg auth key` reads; `None` only for keyless profiles.
    pub keychain_service: Option<String>,
    /// haiku, sonnet, opus → model ids.
    pub models: [String; 3],
}

impl Provider {
    pub fn model(&self, tier: Tier) -> &str {
        &self.models[tier.index()]
    }

    pub fn subscription() -> Provider {
        Provider {
            id: "subscription".into(),
            kind: Kind::Subscription,
            keychain_service: None,
            models: ["haiku".into(), "sonnet".into(), "opus".into()],
        }
    }

    /// A tier means the same model only on Anthropic's own paths (design §5.3 step 4).
    pub fn first_party(&self) -> bool {
        !matches!(self.kind, Kind::Gateway { .. })
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ProviderCfg {
    kind: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    keychain_service: Option<String>,
    models: Models,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct Models {
    haiku: String,
    sonnet: String,
    opus: String,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct AuthCfg {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    fallback: Option<Vec<String>>,
    #[serde(default)]
    stages: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Auth {
    pub default: String,
    pub fallback: Vec<String>,
    pub stages: BTreeMap<String, String>,
    pub providers: Vec<Provider>,
}

const STAGES: [&str; 6] = ["read", "challenge", "work", "fix", "review", "learn"];

fn name_ok(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn model_ok(s: &str) -> bool {
    (1..=100).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/-[]".contains(c))
}

/// `https://…`, or `http://` on a loopback address only.
pub fn gateway_url(url: &str) -> Result<(), String> {
    let host_of = |rest: &str| {
        let authority = rest.split('/').next().unwrap_or("");
        let host = match authority.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or("").to_string(),
            None => authority.split(':').next().unwrap_or("").to_string(),
        };
        host
    };
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("base_url must not contain whitespace".into());
    }
    if let Some(rest) = url.strip_prefix("https://") {
        return if host_of(rest).is_empty() {
            Err("base_url has no host".into())
        } else {
            Ok(())
        };
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let host = host_of(rest);
        return if host == "127.0.0.1" || host == "localhost" || host == "::1" {
            Ok(())
        } else {
            Err(format!(
                "base_url {url} must be https, or http on a loopback address"
            ))
        };
    }
    Err(format!(
        "base_url {url} must be https, or http on a loopback address"
    ))
}

fn provider(id: &str, c: &ProviderCfg) -> Result<Provider, String> {
    if !name_ok(id) {
        return Err(format!("provider name {id:?} must be 1-32 of a-z 0-9 -"));
    }
    let models = [&c.models.haiku, &c.models.sonnet, &c.models.opus];
    if let Some(m) = models.iter().find(|m| !model_ok(m)) {
        return Err(format!("provider {id}: model id {m:?} is not valid"));
    }
    if let Some(k) = &c.keychain_service {
        if !(1..=100).contains(&k.len()) || k.chars().any(|ch| ch.is_control()) {
            return Err(format!("provider {id}: keychain_service is not valid"));
        }
    }
    let kind = match (c.kind.as_str(), &c.base_url, &c.keychain_service) {
        ("subscription", None, None) => Kind::Subscription,
        ("subscription", ..) => {
            return Err(format!(
                "provider {id}: a subscription profile has no base_url or key"
            ))
        }
        ("anthropic-api", None, Some(_)) => Kind::AnthropicApi,
        ("anthropic-api", ..) => {
            return Err(format!(
                "provider {id}: anthropic-api needs keychain_service and no base_url"
            ))
        }
        ("gateway", Some(url), _) => {
            gateway_url(url).map_err(|e| format!("provider {id}: {e}"))?;
            Kind::Gateway {
                base_url: url.clone(),
            }
        }
        ("gateway", None, _) => return Err(format!("provider {id}: a gateway needs base_url")),
        (other, ..) => return Err(format!("provider {id}: unknown kind {other:?}")),
    };
    Ok(Provider {
        id: id.to_string(),
        kind,
        keychain_service: c.keychain_service.clone(),
        models: models.map(|m| m.clone()),
    })
}

/// Validates the provider tables and the `[auth]` choices against them.
pub fn load(providers: &BTreeMap<String, ProviderCfg>, auth: &AuthCfg) -> Result<Auth, String> {
    let mut list = providers
        .iter()
        .map(|(id, c)| provider(id, c))
        .collect::<Result<Vec<_>, String>>()?;
    if !list.iter().any(|p| p.id == "subscription") && providers.is_empty() {
        list.push(Provider::subscription());
    }
    let defined = |n: &str| list.iter().any(|p| p.id == n);
    let default = auth
        .default
        .clone()
        .unwrap_or_else(|| "subscription".into());
    if !defined(&default) {
        return Err(format!(
            "[auth].default names undefined provider {default:?}"
        ));
    }
    for (stage, p) in &auth.stages {
        if !STAGES.contains(&stage.as_str()) {
            return Err(format!("[auth.stages] has unknown stage {stage:?}"));
        }
        if !defined(p) {
            return Err(format!(
                "[auth.stages].{stage} names undefined provider {p:?}"
            ));
        }
    }
    let fallback = match &auth.fallback {
        Some(f) => {
            if let Some(p) = f.iter().find(|p| !defined(p)) {
                return Err(format!("[auth].fallback names undefined provider {p:?}"));
            }
            f.clone()
        }
        None => ["subscription", "anthropic-api"]
            .into_iter()
            .filter(|p| defined(p))
            .map(String::from)
            .collect(),
    };
    Ok(Auth {
        default,
        fallback,
        stages: auth.stages.clone(),
        providers: list,
    })
}

impl Auth {
    pub fn get(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// The provider a stage's call uses, skipping providers limited earlier in the run when the
    /// stage's provider is in the fallback list. `None`: no usable provider is left.
    pub fn pick(&self, stage: &str, limited: &[String]) -> Option<&Provider> {
        let first = self.stages.get(stage).unwrap_or(&self.default);
        if !limited.contains(first) {
            return self.get(first);
        }
        let pos = self.fallback.iter().position(|p| p == first)?;
        self.fallback[pos + 1..]
            .iter()
            .find(|p| !limited.contains(p))
            .and_then(|p| self.get(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> Result<Auth, String> {
        #[derive(Deserialize)]
        struct F {
            #[serde(default)]
            providers: BTreeMap<String, ProviderCfg>,
            #[serde(default)]
            auth: AuthCfg,
        }
        let f: F = toml::from_str(text).map_err(|e| e.to_string())?;
        load(&f.providers, &f.auth)
    }

    const MODELS: &str = "models = { haiku = \"h\", sonnet = \"s\", opus = \"o\" }";

    #[test]
    fn defaults_to_the_subscription_alone() {
        let a = cfg("").unwrap();
        assert_eq!(a.default, "subscription");
        assert_eq!(a.fallback, ["subscription"]);
        assert_eq!(a.get("subscription").unwrap().model(Tier::Opus), "opus");
    }

    #[test]
    fn each_rule_rejects_one_bad_config() {
        let p = |body: &str| format!("[providers.x]\n{body}\n{MODELS}\n[auth]\ndefault = \"x\"\n");
        let cases = [
            (p("kind = \"gateway\"\nbase_url = \"http://example.com\""), "loopback"),
            (p("kind = \"gateway\""), "needs base_url"),
            (p("kind = \"anthropic-api\""), "needs keychain_service"),
            (p("kind = \"subscription\"\nkeychain_service = \"k\""), "has no base_url or key"),
            (p("kind = \"magic\""), "unknown kind"),
            (
                "[providers.x]\nkind = \"subscription\"\nmodels = { haiku = \"h\", sonnet = \"s\" }\n".into(),
                "missing field",
            ),
            ("[auth]\ndefault = \"nope\"\n".into(), "undefined provider"),
            ("[auth.stages]\nread = \"nope\"\n".into(), "undefined provider"),
            ("[auth.stages]\ndeploy = \"subscription\"\n".into(), "unknown stage"),
            ("[auth]\nfallback = [\"nope\"]\n".into(), "undefined provider"),
            (
                format!("[providers.Bad]\nkind = \"subscription\"\n{MODELS}\n"),
                "provider name",
            ),
        ];
        for (text, want) in cases {
            let err = cfg(&text).unwrap_err();
            assert!(err.contains(want), "{text}\n=> {err}");
        }
    }

    #[test]
    fn gateways_accept_https_and_loopback_http() {
        assert!(gateway_url("https://openrouter.ai/api").is_ok());
        assert!(gateway_url("http://127.0.0.1:4000").is_ok());
        assert!(gateway_url("http://[::1]:4000").is_ok());
        assert!(gateway_url("http://10.0.0.1:4000").is_err());
        assert!(gateway_url("ftp://x").is_err());
    }

    #[test]
    fn fallback_skips_limited_providers_and_stage_pins() {
        let text = format!(
            "[providers.subscription]\nkind = \"subscription\"\n{MODELS}\n\
             [providers.anthropic-api]\nkind = \"anthropic-api\"\nkeychain_service = \"k\"\n{MODELS}\n\
             [providers.openrouter]\nkind = \"gateway\"\nbase_url = \"https://openrouter.ai/api\"\nkeychain_service = \"o\"\n{MODELS}\n\
             [auth.stages]\nlearn = \"openrouter\"\n"
        );
        let a = cfg(&text).unwrap();
        assert_eq!(a.fallback, ["subscription", "anthropic-api"]);
        assert_eq!(a.pick("read", &[]).unwrap().id, "subscription");
        let limited = vec!["subscription".to_string()];
        assert_eq!(a.pick("read", &limited).unwrap().id, "anthropic-api");
        let both = vec!["subscription".to_string(), "anthropic-api".to_string()];
        assert!(a.pick("read", &both).is_none());
        // A stage whose provider is not in the fallback list does not fall back.
        assert!(a.pick("learn", &["openrouter".to_string()]).is_none());
        assert!(!a.get("openrouter").unwrap().first_party());
    }
}
