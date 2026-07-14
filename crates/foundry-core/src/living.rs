use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeLayer {
    Observed,
    Inferred,
    Legislated,
    Historical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub uri: String,
    pub digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedAssumption {
    pub name: String,
    pub statement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transformation {
    pub name: String,
    pub version: String,
    pub input_digests: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RetentionPolicy {
    DeleteAfter { at: DateTime<Utc> },
    ReviewAfter { at: DateTime<Utc> },
    Preserve { basis: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernanceEnvelope {
    pub layer: KnowledgeLayer,
    pub sources: Vec<SourceRef>,
    pub assumptions: Vec<NamedAssumption>,
    pub transformation: Transformation,
    pub owner: String,
    pub retention: RetentionPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Retain,
    Review,
    Delete,
}

impl GovernanceEnvelope {
    pub fn validate(&self) -> Result<(), ConformanceError> {
        if self.sources.is_empty() {
            return Err(ConformanceError::MissingSources);
        }
        if self
            .sources
            .iter()
            .any(|source| source.uri.trim().is_empty())
        {
            return Err(ConformanceError::UnnamedSource);
        }
        if self.owner.trim().is_empty() {
            return Err(ConformanceError::MissingOwner);
        }
        if self.assumptions.iter().any(|assumption| {
            assumption.name.trim().is_empty() || assumption.statement.trim().is_empty()
        }) {
            return Err(ConformanceError::UnnamedAssumption);
        }
        if self.transformation.name.trim().is_empty()
            || self.transformation.version.trim().is_empty()
        {
            return Err(ConformanceError::UnnamedTransformation);
        }
        if let RetentionPolicy::Preserve { basis } = &self.retention
            && basis.trim().is_empty()
        {
            return Err(ConformanceError::MissingPreservationBasis);
        }
        Ok(())
    }

    pub fn disposition_at(&self, now: DateTime<Utc>) -> Disposition {
        match self.retention {
            RetentionPolicy::DeleteAfter { at } if at <= now => Disposition::Delete,
            RetentionPolicy::ReviewAfter { at } if at <= now => Disposition::Review,
            _ => Disposition::Retain,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConformanceError {
    #[error("derived evidence must name at least one source")]
    MissingSources,
    #[error("source references must have a URI")]
    UnnamedSource,
    #[error("derived evidence must have an owner")]
    MissingOwner,
    #[error("assumptions must have a name and statement")]
    UnnamedAssumption,
    #[error("transformations must have a name and version")]
    UnnamedTransformation,
    #[error("preserved evidence must state its preservation basis")]
    MissingPreservationBasis,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn envelope(retention: RetentionPolicy) -> GovernanceEnvelope {
        GovernanceEnvelope {
            layer: KnowledgeLayer::Observed,
            sources: vec![SourceRef {
                uri: "job://attempt-1/stdout".into(),
                digest: Some("sha256:abc".into()),
            }],
            assumptions: vec![NamedAssumption {
                name: "clean runner".into(),
                statement: "the runner image matches its declared digest".into(),
            }],
            transformation: Transformation {
                name: "capture-job-result".into(),
                version: "1".into(),
                input_digests: vec!["sha256:input".into()],
            },
            owner: "foundry-runner".into(),
            retention,
        }
    }

    #[test]
    fn conformance_requires_named_provenance() {
        let mut value = envelope(RetentionPolicy::Preserve {
            basis: "release audit".into(),
        });
        value.sources.clear();
        assert_eq!(value.validate(), Err(ConformanceError::MissingSources));
    }

    #[test]
    fn conformance_makes_forgetting_executable() {
        let now = Utc::now();
        let value = envelope(RetentionPolicy::DeleteAfter {
            at: now - TimeDelta::seconds(1),
        });
        assert_eq!(value.validate(), Ok(()));
        assert_eq!(value.disposition_at(now), Disposition::Delete);
    }

    #[test]
    fn conformance_requires_a_basis_for_preservation() {
        let value = envelope(RetentionPolicy::Preserve { basis: " ".into() });
        assert_eq!(
            value.validate(),
            Err(ConformanceError::MissingPreservationBasis)
        );
    }
}
