use crate::auth::policy::{matches_wildcard, ActionValue, ResourceValue};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyResult {
    Allow,
    ExplicitDeny,
    NotApplicable,
}

/// S3 Bucket Policy JSON document.
/// Stored at /{bucket}/.s3_bucket_policy in DFS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketPolicyDocument {
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "Statement")]
    pub statements: Vec<BucketStatement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketStatement {
    #[serde(rename = "Effect")]
    pub effect: String,
    #[serde(rename = "Principal")]
    pub principal: PrincipalValue,
    #[serde(rename = "Action")]
    pub actions: ActionValue,
    #[serde(rename = "Resource", skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceValue>,
}

/// Principal value: either "*" (everyone) or {"AWS": "arn:..."} or a bare ARN string
#[derive(Debug, Clone)]
pub enum PrincipalValue {
    Wildcard,
    Aws(AwsPrincipal),
}

impl serde::Serialize for PrincipalValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            PrincipalValue::Wildcard => serializer.serialize_str("*"),
            PrincipalValue::Aws(p) => p.serialize(serializer),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AwsPrincipal {
    Single(String),
    Map {
        #[serde(rename = "AWS")]
        aws: StringOrVec,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StringOrVec {
    Single(String),
    Multiple(Vec<String>),
}

impl<'de> serde::de::Deserialize<'de> for PrincipalValue {
    fn deserialize<D: serde::de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        match &value {
            serde_json::Value::String(s) if s == "*" => Ok(PrincipalValue::Wildcard),
            serde_json::Value::String(_) | serde_json::Value::Object(_) => {
                let aws_principal =
                    AwsPrincipal::deserialize(value).map_err(|e| Error::custom(e.to_string()))?;
                Ok(PrincipalValue::Aws(aws_principal))
            }
            _ => Err(Error::custom("invalid Principal value")),
        }
    }
}

impl PrincipalValue {
    pub fn matches(&self, principal_arn: Option<&str>) -> bool {
        match self {
            PrincipalValue::Wildcard => true,
            PrincipalValue::Aws(AwsPrincipal::Single(s)) => {
                if s == "*" {
                    return true;
                }
                principal_arn
                    .map(|p| matches_wildcard(s, p))
                    .unwrap_or(false)
            }
            PrincipalValue::Aws(AwsPrincipal::Map { aws }) => match aws {
                StringOrVec::Single(s) => {
                    if s == "*" {
                        return true;
                    }
                    principal_arn
                        .map(|p| matches_wildcard(s, p))
                        .unwrap_or(false)
                }
                StringOrVec::Multiple(v) => v.iter().any(|s| {
                    if s == "*" {
                        return true;
                    }
                    principal_arn
                        .map(|p| matches_wildcard(s, p))
                        .unwrap_or(false)
                }),
            },
        }
    }
}

pub struct BucketPolicyEvaluator {
    policy: BucketPolicyDocument,
}

impl BucketPolicyEvaluator {
    pub fn new(policy: BucketPolicyDocument) -> Self {
        Self { policy }
    }

    /// Evaluate whether `principal_arn` (None = unauthenticated) can perform `action` on `resource`.
    /// Returns ExplicitDeny if any Deny statement matches, Allow if any Allow matches, NotApplicable otherwise.
    pub fn evaluate(
        &self,
        principal_arn: Option<&str>,
        action: &str,
        resource: &str,
    ) -> PolicyResult {
        let mut allow = false;

        for stmt in &self.policy.statements {
            // 1. Principal match
            if !stmt.principal.matches(principal_arn) {
                continue;
            }

            // 2. Action match
            if !stmt.actions.contains(action) {
                continue;
            }

            // 3. Resource match
            if let Some(resources) = &stmt.resources {
                if !resources.contains(resource) {
                    continue;
                }
            }

            // 4. Effect — Deny always wins immediately
            if stmt.effect == "Deny" {
                return PolicyResult::ExplicitDeny;
            } else if stmt.effect == "Allow" {
                allow = true;
            }
        }

        if allow {
            PolicyResult::Allow
        } else {
            PolicyResult::NotApplicable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_policy(
        effect: &str,
        principal: &str,
        action: &str,
        resource: &str,
    ) -> BucketPolicyDocument {
        let principal_val = if principal == "*" {
            PrincipalValue::Wildcard
        } else {
            PrincipalValue::Aws(AwsPrincipal::Single(principal.to_string()))
        };
        BucketPolicyDocument {
            version: "2012-10-17".to_string(),
            statements: vec![BucketStatement {
                effect: effect.to_string(),
                principal: principal_val,
                actions: ActionValue::Single(action.to_string()),
                resources: Some(ResourceValue::Single(resource.to_string())),
            }],
        }
    }

    #[test]
    fn allow_wildcard_principal() {
        let policy = make_policy("Allow", "*", "s3:GetObject", "arn:dfs:s3:::mybucket/*");
        let evaluator = BucketPolicyEvaluator::new(policy);
        assert_eq!(
            evaluator.evaluate(None, "s3:GetObject", "arn:dfs:s3:::mybucket/key"),
            PolicyResult::Allow
        );
    }

    #[test]
    fn deny_explicit() {
        let policy = make_policy("Deny", "*", "s3:DeleteObject", "arn:dfs:s3:::mybucket/*");
        let evaluator = BucketPolicyEvaluator::new(policy);
        assert_eq!(
            evaluator.evaluate(None, "s3:DeleteObject", "arn:dfs:s3:::mybucket/key"),
            PolicyResult::ExplicitDeny
        );
    }

    #[test]
    fn allow_specific_principal() {
        let policy = make_policy(
            "Allow",
            "arn:dfs:iam:::role/ReadOnly",
            "s3:GetObject",
            "arn:dfs:s3:::mybucket/*",
        );
        let evaluator = BucketPolicyEvaluator::new(policy);
        assert_eq!(
            evaluator.evaluate(
                Some("arn:dfs:iam:::role/ReadOnly"),
                "s3:GetObject",
                "arn:dfs:s3:::mybucket/key"
            ),
            PolicyResult::Allow
        );
        assert_eq!(
            evaluator.evaluate(
                Some("arn:dfs:iam:::role/Other"),
                "s3:GetObject",
                "arn:dfs:s3:::mybucket/key"
            ),
            PolicyResult::NotApplicable
        );
    }

    #[test]
    fn not_applicable_when_no_match() {
        let policy = make_policy("Allow", "*", "s3:PutObject", "arn:dfs:s3:::mybucket/*");
        let evaluator = BucketPolicyEvaluator::new(policy);
        assert_eq!(
            evaluator.evaluate(None, "s3:GetObject", "arn:dfs:s3:::mybucket/key"),
            PolicyResult::NotApplicable
        );
    }

    #[test]
    fn parse_json_roundtrip() {
        let json = r#"{
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Principal": "*",
                "Action": "s3:GetObject",
                "Resource": "arn:dfs:s3:::mybucket/*"
            }]
        }"#;
        let doc: BucketPolicyDocument = serde_json::from_str(json).unwrap();
        assert_eq!(doc.statements.len(), 1);
        let back = serde_json::to_string(&doc).unwrap();
        let doc2: BucketPolicyDocument = serde_json::from_str(&back).unwrap();
        assert_eq!(doc2.statements.len(), 1);
    }
}
