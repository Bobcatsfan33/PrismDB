//! Tenant authorization, local rate limits, and a durable usage ledger for the
//! production model plane.
//!
//! This wrapper sits below ingest, direct query, SQL, migration, and evaluation
//! call sites. Enabling it changes the default from implicit allow to exact
//! tenant + model + version + purpose grants.

use crate::model::ModelPlane;
use prism_types::error::{PrismError, Result};
use prism_types::{
    Embedder, EmbeddingInput, EmbeddingPurpose, ModelArtifacts, MAX_EMBED_INPUT_BYTES,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_POLICY_BYTES: u64 = 1024 * 1024;
const MAX_TENANTS: usize = 10_000;
const MAX_GRANTS_PER_TENANT: usize = 128;
const MAX_LIMIT: u64 = 1_000_000_000;
const WINDOW_MS: i64 = 60_000;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPolicy {
    pub schema_version: u32,
    pub default_action: String,
    pub tenants: Vec<TenantPolicy>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantPolicy {
    pub tenant_id: String,
    pub grants: Vec<ModelGrant>,
    pub max_inputs_per_minute: u64,
    pub max_input_bytes_per_minute: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelGrant {
    pub model_id: String,
    pub model_version: String,
    pub purposes: Vec<EmbeddingPurpose>,
}

impl ModelPolicy {
    pub fn load(path: &Path) -> Result<Self> {
        if path.is_symlink() {
            return Err(PrismError::Invalid(
                "model policy path must not be a symlink".into(),
            ));
        }
        let metadata = path.metadata()?;
        if !metadata.is_file() {
            return Err(PrismError::Invalid(
                "model policy path must be a regular file".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(PrismError::Invalid(format!(
                    "model policy {} has mode {mode:04o}; group/other access is forbidden",
                    path.display()
                )));
            }
        }
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_POLICY_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_POLICY_BYTES {
            return Err(PrismError::Invalid(format!(
                "model policy exceeds {MAX_POLICY_BYTES} bytes"
            )));
        }
        let policy: Self = serde_json::from_slice(&bytes)?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            return Err(PrismError::Invalid(format!(
                "model policy schema_version must be 1, got {}",
                self.schema_version
            )));
        }
        if self.default_action != "deny" {
            return Err(PrismError::Invalid(
                "model policy default_action must be `deny`; implicit allow is forbidden".into(),
            ));
        }
        if self.tenants.is_empty() || self.tenants.len() > MAX_TENANTS {
            return Err(PrismError::Invalid(format!(
                "model policy must contain 1..={MAX_TENANTS} tenants"
            )));
        }
        let mut tenant_ids = BTreeSet::new();
        for tenant in &self.tenants {
            if tenant.tenant_id.trim().is_empty() || tenant.tenant_id.len() > 256 {
                return Err(PrismError::Invalid(
                    "model policy tenant_id must contain 1..=256 characters".into(),
                ));
            }
            if !tenant_ids.insert(tenant.tenant_id.as_str()) {
                return Err(PrismError::Invalid(format!(
                    "duplicate model policy tenant `{}`",
                    tenant.tenant_id
                )));
            }
            if tenant.grants.is_empty() || tenant.grants.len() > MAX_GRANTS_PER_TENANT {
                return Err(PrismError::Invalid(format!(
                    "tenant `{}` must contain 1..={MAX_GRANTS_PER_TENANT} model grants",
                    tenant.tenant_id
                )));
            }
            for (name, limit) in [
                ("max_inputs_per_minute", tenant.max_inputs_per_minute),
                (
                    "max_input_bytes_per_minute",
                    tenant.max_input_bytes_per_minute,
                ),
            ] {
                if limit == 0 || limit > MAX_LIMIT {
                    return Err(PrismError::Invalid(format!(
                        "tenant `{}` {name} must be in 1..={MAX_LIMIT}",
                        tenant.tenant_id
                    )));
                }
            }
            let mut grants = BTreeSet::new();
            for grant in &tenant.grants {
                if grant.model_id.trim().is_empty()
                    || grant.model_id.len() > 256
                    || grant.model_version.len() != 64
                    || !grant
                        .model_version
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(PrismError::Invalid(format!(
                        "tenant `{}` has an invalid exact model grant",
                        tenant.tenant_id
                    )));
                }
                if grant.purposes.is_empty() {
                    return Err(PrismError::Invalid(format!(
                        "tenant `{}` model grant must name at least one purpose",
                        tenant.tenant_id
                    )));
                }
                let purpose_key = grant
                    .purposes
                    .iter()
                    .map(|purpose| purpose.as_str())
                    .collect::<BTreeSet<_>>();
                if purpose_key.len() != grant.purposes.len()
                    || !grants.insert((grant.model_id.as_str(), grant.model_version.as_str()))
                {
                    return Err(PrismError::Invalid(format!(
                        "tenant `{}` has duplicate model grants or purposes",
                        tenant.tenant_id
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
struct WindowUsage {
    start_ms: i64,
    inputs: u64,
    bytes: u64,
}

#[derive(Clone, Debug)]
struct Prepared {
    text: String,
    input_bytes: usize,
    tenant_id: String,
    purpose: EmbeddingPurpose,
}

#[derive(Serialize)]
struct AuditEvent<'a> {
    schema_version: u32,
    timestamp_ms: i64,
    tenant_id: &'a str,
    purpose: &'a str,
    model_id: &'a str,
    model_version: &'a str,
    input_bytes: usize,
    outcome: &'a str,
}

pub struct ModelPolicyEnforcer {
    tenants: BTreeMap<String, TenantPolicy>,
    usage: Mutex<BTreeMap<(String, String, String), WindowUsage>>,
    audit: Mutex<File>,
}

impl ModelPolicyEnforcer {
    pub fn new(policy: ModelPolicy, audit_path: &Path) -> Result<Self> {
        policy.validate()?;
        if !audit_path.is_absolute() {
            return Err(PrismError::Invalid(
                "model usage audit path must be absolute".into(),
            ));
        }
        let parent = audit_path.parent().ok_or_else(|| {
            PrismError::Invalid("model usage audit path has no parent directory".into())
        })?;
        if !parent.is_dir() {
            return Err(PrismError::Invalid(format!(
                "model usage audit parent {} does not exist",
                parent.display()
            )));
        }
        if audit_path.is_symlink() {
            return Err(PrismError::Invalid(
                "model usage audit path must not be a symlink".into(),
            ));
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let audit = options.open(audit_path)?;
        if !audit.metadata()?.is_file() {
            return Err(PrismError::Invalid(
                "model usage audit path must be a regular file".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = audit.metadata()?.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(PrismError::Invalid(format!(
                    "model usage audit log {} has mode {mode:04o}; group/other access is forbidden",
                    audit_path.display()
                )));
            }
        }
        Ok(Self {
            tenants: policy
                .tenants
                .into_iter()
                .map(|tenant| (tenant.tenant_id.clone(), tenant))
                .collect(),
            usage: Mutex::new(BTreeMap::new()),
            audit: Mutex::new(audit),
        })
    }

    fn prepare(
        &self,
        model_id: &str,
        model_version: &str,
        input: EmbeddingInput<'_>,
    ) -> Result<Prepared> {
        let tenant = self.authorize(model_id, model_version, input)?;
        let tenant_id = input.tenant_id.expect("authorize requires tenant context");
        let input_bytes = input.text.len();
        if input.purpose != EmbeddingPurpose::Ingest {
            self.charge(tenant, model_id, model_version, input_bytes as u64)?;
        }
        Ok(Prepared {
            text: input.text.to_string(),
            input_bytes,
            tenant_id: tenant_id.to_string(),
            purpose: input.purpose,
        })
    }

    fn authorize<'a>(
        &'a self,
        model_id: &str,
        model_version: &str,
        input: EmbeddingInput<'_>,
    ) -> Result<&'a TenantPolicy> {
        let tenant_id = input.tenant_id.ok_or_else(|| {
            PrismError::Policy(
                "production model policy requires an authenticated tenant context".into(),
            )
        })?;
        let tenant = self.tenants.get(tenant_id).ok_or_else(|| {
            PrismError::Policy(format!(
                "tenant `{tenant_id}` has no production model policy"
            ))
        })?;
        let authorized = tenant.grants.iter().any(|grant| {
            grant.model_id == model_id
                && grant.model_version == model_version
                && grant.purposes.contains(&input.purpose)
        });
        if !authorized {
            return Err(PrismError::Policy(format!(
                "tenant `{tenant_id}` is not authorized for model `{model_id}:{model_version}` \
                 purpose `{}`",
                input.purpose.as_str()
            )));
        }
        let input_bytes = input.text.len();
        if input_bytes > MAX_EMBED_INPUT_BYTES {
            return Err(PrismError::Policy(format!(
                "tenant `{tenant_id}` embedding input is {input_bytes} bytes; limit is \
                 {MAX_EMBED_INPUT_BYTES}"
            )));
        }
        Ok(tenant)
    }

    fn preflight(
        &self,
        model_id: &str,
        model_version: &str,
        input: EmbeddingInput<'_>,
    ) -> Result<()> {
        let tenant = self.authorize(model_id, model_version, input)?;
        self.charge(tenant, model_id, model_version, input.text.len() as u64)
    }

    fn charge(
        &self,
        tenant: &TenantPolicy,
        model_id: &str,
        model_version: &str,
        bytes: u64,
    ) -> Result<()> {
        let now = crate::clock::lease_now_ms();
        let key = (
            tenant.tenant_id.clone(),
            model_id.to_string(),
            model_version.to_string(),
        );
        let mut usage = self
            .usage
            .lock()
            .map_err(|_| PrismError::Invariant("model usage lock poisoned".into()))?;
        let current = usage.entry(key).or_insert(WindowUsage {
            start_ms: now,
            ..Default::default()
        });
        if now.saturating_sub(current.start_ms) >= WINDOW_MS {
            *current = WindowUsage {
                start_ms: now,
                ..Default::default()
            };
        }
        if current.inputs + 1 > tenant.max_inputs_per_minute
            || current.bytes.saturating_add(bytes) > tenant.max_input_bytes_per_minute
        {
            return Err(PrismError::Policy(format!(
                "tenant `{}` exceeded its local model input or byte budget for this minute",
                tenant.tenant_id
            )));
        }
        current.inputs += 1;
        current.bytes += bytes;
        Ok(())
    }

    fn audit(
        &self,
        model_id: &str,
        model_version: &str,
        prepared: &Prepared,
        outcome: &str,
    ) -> Result<()> {
        self.write_audit(AuditEvent {
            schema_version: 1,
            timestamp_ms: now_ms(),
            tenant_id: &prepared.tenant_id,
            purpose: prepared.purpose.as_str(),
            model_id,
            model_version,
            input_bytes: prepared.input_bytes,
            outcome,
        })
    }

    fn audit_denied(
        &self,
        model_id: &str,
        model_version: &str,
        input: EmbeddingInput<'_>,
    ) -> Result<()> {
        self.write_audit(AuditEvent {
            schema_version: 1,
            timestamp_ms: now_ms(),
            tenant_id: input.tenant_id.unwrap_or("<missing>"),
            purpose: input.purpose.as_str(),
            model_id,
            model_version,
            input_bytes: input.text.len(),
            outcome: "denied",
        })
    }

    fn write_audit(&self, event: AuditEvent<'_>) -> Result<()> {
        let mut audit = self
            .audit
            .lock()
            .map_err(|_| PrismError::Invariant("model audit lock poisoned".into()))?;
        serde_json::to_writer(&mut *audit, &event)?;
        audit.write_all(b"\n")?;
        Ok(())
    }

    fn sync_audit(&self) -> Result<()> {
        self.audit
            .lock()
            .map_err(|_| PrismError::Invariant("model audit lock poisoned".into()))?
            .sync_data()?;
        Ok(())
    }
}

pub struct GovernedModelPlane {
    inner: Arc<dyn ModelPlane>,
    enforcer: Arc<ModelPolicyEnforcer>,
}

impl GovernedModelPlane {
    pub fn new(inner: Arc<dyn ModelPlane>, enforcer: Arc<ModelPolicyEnforcer>) -> Self {
        Self { inner, enforcer }
    }

    fn wrap(&self, inner: Arc<dyn Embedder>) -> Arc<dyn Embedder> {
        Arc::new(GovernedEmbedder {
            inner,
            enforcer: self.enforcer.clone(),
        })
    }
}

impl ModelPlane for GovernedModelPlane {
    fn preflight(
        &self,
        model_id: &str,
        model_version: &str,
        inputs: &[EmbeddingInput<'_>],
    ) -> Vec<Result<()>> {
        let mut results = Vec::with_capacity(inputs.len());
        let mut audit_error: Option<String> = None;
        for input in inputs.iter().copied() {
            match self.enforcer.preflight(model_id, model_version, input) {
                Ok(()) => results.push(Ok(())),
                Err(error) => {
                    if let Err(error) = self.enforcer.audit_denied(model_id, model_version, input) {
                        audit_error = Some(error.to_string());
                    }
                    results.push(Err(error));
                }
            }
        }
        if audit_error.is_none() {
            if let Err(error) = self.enforcer.sync_audit() {
                audit_error = Some(error.to_string());
            }
        }
        if let Some(error) = audit_error {
            return inputs
                .iter()
                .map(|_| {
                    Err(PrismError::Io(format!(
                        "model usage audit failed closed: {error}"
                    )))
                })
                .collect();
        }
        results
    }

    fn embedder(
        &self,
        model_id: &str,
        model_version: &str,
        dim: usize,
        expected_artifacts: Option<&ModelArtifacts>,
    ) -> Result<Arc<dyn Embedder>> {
        self.inner
            .embedder(model_id, model_version, dim, expected_artifacts)
            .map(|embedder| self.wrap(embedder))
    }

    fn candidate_embedder(
        &self,
        model_id: &str,
        model_version: &str,
        dim: usize,
    ) -> Result<Arc<dyn Embedder>> {
        self.inner
            .candidate_embedder(model_id, model_version, dim)
            .map(|embedder| self.wrap(embedder))
    }

    fn default_embedder(&self, dim: usize) -> Result<Arc<dyn Embedder>> {
        self.inner
            .default_embedder(dim)
            .map(|embedder| self.wrap(embedder))
    }
}

struct GovernedEmbedder {
    inner: Arc<dyn Embedder>,
    enforcer: Arc<ModelPolicyEnforcer>,
}

impl Embedder for GovernedEmbedder {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn model_version(&self) -> &str {
        self.inner.model_version()
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    fn artifacts(&self) -> Option<&ModelArtifacts> {
        self.inner.artifacts()
    }

    fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        Err(PrismError::Policy(
            "production model policy requires embed_scoped tenant context".into(),
        ))
    }

    fn embed_batch(&self, texts: &[&str]) -> Vec<Result<Vec<f32>>> {
        texts
            .iter()
            .map(|_| {
                Err(PrismError::Policy(
                    "production model policy requires embed_batch_scoped tenant context".into(),
                ))
            })
            .collect()
    }

    fn embed_scoped(&self, input: EmbeddingInput<'_>) -> Result<Vec<f32>> {
        self.embed_batch_scoped(&[input])
            .pop()
            .expect("one scoped input produces one result")
    }

    fn embed_batch_scoped(&self, inputs: &[EmbeddingInput<'_>]) -> Vec<Result<Vec<f32>>> {
        let mut results: Vec<Option<Result<Vec<f32>>>> =
            std::iter::repeat_with(|| None).take(inputs.len()).collect();
        let mut prepared = Vec::new();
        let mut audit_error: Option<String> = None;
        for (index, input) in inputs.iter().copied().enumerate() {
            match self
                .enforcer
                .prepare(self.model_id(), self.model_version(), input)
            {
                Ok(value) => prepared.push((index, value)),
                Err(error) => {
                    if let Err(error) =
                        self.enforcer
                            .audit_denied(self.model_id(), self.model_version(), input)
                    {
                        audit_error = Some(error.to_string());
                    }
                    results[index] = Some(Err(error));
                }
            }
        }

        let texts: Vec<&str> = prepared
            .iter()
            .map(|(_, item)| item.text.as_str())
            .collect();
        let inferred = self.inner.embed_batch(&texts);
        if inferred.len() != prepared.len() {
            let message = format!(
                "governed embedder returned {} results for {} authorized inputs",
                inferred.len(),
                prepared.len()
            );
            for (index, _) in &prepared {
                results[*index] = Some(Err(PrismError::Invariant(message.clone())));
            }
        } else {
            for ((index, _), result) in prepared.iter().zip(inferred) {
                results[*index] = Some(result);
            }
        }

        for (index, item) in &prepared {
            let outcome = match &results[*index] {
                Some(Ok(_)) => "ok",
                _ => "error",
            };
            if let Err(error) =
                self.enforcer
                    .audit(self.model_id(), self.model_version(), item, outcome)
            {
                audit_error = Some(error.to_string());
            }
        }
        if audit_error.is_none() {
            if let Err(error) = self.enforcer.sync_audit() {
                audit_error = Some(error.to_string());
            }
        }
        if let Some(error) = audit_error {
            for result in &mut results {
                *result = Some(Err(PrismError::Io(format!(
                    "model usage audit failed closed: {error}"
                ))));
            }
        }
        results
            .into_iter()
            .map(|result| {
                result.unwrap_or_else(|| {
                    Err(PrismError::Invariant(
                        "model policy lost an input result".into(),
                    ))
                })
            })
            .collect()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    const VERSION: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    struct CaptureEmbedder {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Embedder for CaptureEmbedder {
        fn model_id(&self) -> &str {
            "approved"
        }

        fn model_version(&self) -> &str {
            VERSION
        }

        fn dim(&self) -> usize {
            2
        }

        fn embed(&self, text: &str) -> Result<Vec<f32>> {
            self.seen.lock().unwrap().push(text.to_string());
            Ok(vec![1.0, 0.0])
        }
    }

    fn policy(max_inputs: u64, purposes: Vec<EmbeddingPurpose>) -> ModelPolicy {
        ModelPolicy {
            schema_version: 1,
            default_action: "deny".into(),
            tenants: vec![TenantPolicy {
                tenant_id: "acme".into(),
                grants: vec![ModelGrant {
                    model_id: "approved".into(),
                    model_version: VERSION.into(),
                    purposes,
                }],
                max_inputs_per_minute: max_inputs,
                max_input_bytes_per_minute: 4096,
            }],
        }
    }

    fn governed(
        policy: ModelPolicy,
    ) -> (
        GovernedEmbedder,
        Arc<Mutex<Vec<String>>>,
        std::path::PathBuf,
    ) {
        let root = std::env::temp_dir().join(format!(
            "prism-model-policy-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let audit_path = root.join("usage.jsonl");
        let enforcer =
            Arc::new(ModelPolicyEnforcer::new(policy, &audit_path).expect("valid policy"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            GovernedEmbedder {
                inner: Arc::new(CaptureEmbedder { seen: seen.clone() }),
                enforcer,
            },
            seen,
            audit_path,
        )
    }

    #[test]
    fn authorizes_inference_and_never_audits_text() {
        let (embedder, seen, audit_path) = governed(policy(10, vec![EmbeddingPurpose::Ingest]));
        let results = embedder.embed_batch_scoped(&[
            EmbeddingInput {
                tenant_id: Some("acme"),
                purpose: EmbeddingPurpose::Ingest,
                text: "send customer-secret now",
            },
            EmbeddingInput {
                tenant_id: Some("unknown"),
                purpose: EmbeddingPurpose::Ingest,
                text: "customer-secret must not leak",
            },
        ]);
        assert!(results[0].is_ok());
        assert!(results[1].is_err());
        assert_eq!(
            *seen.lock().unwrap(),
            ["send customer-secret now".to_string()]
        );
        let audit = std::fs::read_to_string(&audit_path).unwrap();
        assert!(!audit.contains("customer-secret"));
        assert!(audit.contains("\"outcome\":\"denied\""));
        std::fs::remove_dir_all(audit_path.parent().unwrap()).ok();
    }

    #[test]
    fn exact_purpose_grant_and_local_rate_limit_fail_closed() {
        let (embedder, seen, audit_path) = governed(policy(1, vec![EmbeddingPurpose::Ingest]));
        let wrong_purpose = embedder.embed_scoped(EmbeddingInput {
            tenant_id: Some("acme"),
            purpose: EmbeddingPurpose::Query,
            text: "query",
        });
        assert!(wrong_purpose
            .unwrap_err()
            .to_string()
            .contains("not authorized"));

        let first = EmbeddingInput {
            tenant_id: Some("acme"),
            purpose: EmbeddingPurpose::Ingest,
            text: "one",
        };
        let second = EmbeddingInput {
            tenant_id: Some("acme"),
            purpose: EmbeddingPurpose::Ingest,
            text: "two",
        };
        assert!(embedder
            .enforcer
            .preflight(embedder.model_id(), embedder.model_version(), first)
            .is_ok());
        assert!(embedder
            .enforcer
            .preflight(embedder.model_id(), embedder.model_version(), second)
            .unwrap_err()
            .to_string()
            .contains("exceeded"));
        assert!(embedder.embed_scoped(first).is_ok());
        assert_eq!(*seen.lock().unwrap(), ["one".to_string()]);
        std::fs::remove_dir_all(audit_path.parent().unwrap()).ok();
    }

    #[test]
    fn unscoped_calls_and_implicit_allow_are_refused() {
        let (embedder, _, audit_path) = governed(policy(10, vec![EmbeddingPurpose::Ingest]));
        assert!(embedder
            .embed("missing tenant")
            .unwrap_err()
            .to_string()
            .contains("tenant context"));
        std::fs::remove_dir_all(audit_path.parent().unwrap()).ok();

        let mut invalid = policy(10, vec![EmbeddingPurpose::Ingest]);
        invalid.default_action = "allow".into();
        assert!(invalid
            .validate()
            .unwrap_err()
            .to_string()
            .contains("default_action"));
    }
}
