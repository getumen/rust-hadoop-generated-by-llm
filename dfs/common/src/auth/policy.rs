use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct EvaluationContext {
    pub principal_id: String,
    pub groups: Vec<String>,
    pub claims: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IamConfig {
    #[serde(rename = "Roles")]
    pub roles: Vec<Role>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Role {
    #[serde(rename = "RoleName")]
    pub role_name: String,
    #[serde(rename = "Arn")]
    pub arn: String,
    #[serde(rename = "AssumeRolePolicyDocument")]
    pub assume_role_policy: PolicyDocument,
    #[serde(rename = "Policies")]
    pub policies: Vec<ManagedPolicy>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ManagedPolicy {
    #[serde(rename = "PolicyName")]
    pub policy_name: String,
    #[serde(rename = "PolicyDocument")]
    pub policy_document: PolicyDocument,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PolicyDocument {
    #[serde(rename = "Statement")]
    pub statements: Vec<Statement>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Statement {
    #[serde(rename = "Effect")]
    pub effect: String, // "Allow" or "Deny"
    #[serde(rename = "Action")]
    pub actions: ActionValue,
    #[serde(rename = "Resource", skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceValue>,
    #[serde(rename = "Condition", skip_serializing_if = "Option::is_none")]
    pub condition: Option<BTreeMap<String, BTreeMap<String, Vec<String>>>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum ActionValue {
    Single(String),
    Multiple(Vec<String>),
}

impl ActionValue {
    pub fn contains(&self, action: &str) -> bool {
        match self {
            ActionValue::Single(s) => matches_wildcard(s, action),
            ActionValue::Multiple(v) => v.iter().any(|s| matches_wildcard(s, action)),
        }
    }
}

pub fn matches_wildcard(pattern: &str, target: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let regex_pattern = pattern
        .replace(".", "\\.")
        .replace("*", ".*")
        .replace("?", ".");
    let regex = format!("^{}$", regex_pattern);
    if let Ok(re) = regex::Regex::new(&regex) {
        re.is_match(target)
    } else {
        pattern == target
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum ResourceValue {
    Single(String),
    Multiple(Vec<String>),
}

impl ResourceValue {
    pub fn contains(&self, resource: &str) -> bool {
        match self {
            ResourceValue::Single(s) => matches_wildcard(s, resource),
            ResourceValue::Multiple(v) => v.iter().any(|s| matches_wildcard(s, resource)),
        }
    }
}

pub struct PolicyEvaluator {
    config: IamConfig,
}

impl PolicyEvaluator {
    pub fn new(config: IamConfig) -> Self {
        Self { config }
    }

    pub fn can_assume_role(&self, role_arn: &str, context: &EvaluationContext) -> bool {
        let role = match self.config.roles.iter().find(|r| r.arn == role_arn) {
            Some(r) => r,
            None => return false,
        };

        // Evaluate Trust Policy (AssumeRolePolicyDocument)
        // For simplicity in this CLI-based Hadoop context, we primarily look at Conditions (groups/claims)
        self.evaluate_statements(
            &role.assume_role_policy.statements,
            "sts:AssumeRoleWithWebIdentity",
            "*",
            context,
        )
    }

    pub fn evaluate(
        &self,
        action: &str,
        resource: &str,
        role_arn: &str,
        context: &EvaluationContext,
    ) -> bool {
        let role = match self.config.roles.iter().find(|r| r.arn == role_arn) {
            Some(r) => r,
            None => return false,
        };

        // Combine all policies attached to the role
        let all_statements: Vec<&Statement> = role
            .policies
            .iter()
            .flat_map(|p| &p.policy_document.statements)
            .collect();

        self.evaluate_statements(&all_statements, action, resource, context)
    }

    fn evaluate_statements(
        &self,
        statements: &[impl std::borrow::Borrow<Statement>],
        action: &str,
        resource: &str,
        context: &EvaluationContext,
    ) -> bool {
        let mut allow = false;

        for stmt_ref in statements {
            let stmt = stmt_ref.borrow();

            // 1. Check Action
            if !stmt.actions.contains(action) {
                continue;
            }

            // 2. Check Resource (if applicable)
            if let Some(resources) = &stmt.resources {
                if !resources.contains(resource) {
                    continue;
                }
            }

            // 3. Check Condition
            if let Some(condition) = &stmt.condition {
                if !self.evaluate_condition(condition, context) {
                    continue;
                }
            }

            // 4. Handle Effect
            if stmt.effect == "Deny" {
                return false; // Explicit Deny always wins
            } else if stmt.effect == "Allow" {
                allow = true;
            }
        }

        allow
    }

    fn evaluate_condition(
        &self,
        condition: &BTreeMap<String, BTreeMap<String, Vec<String>>>,
        context: &EvaluationContext,
    ) -> bool {
        for (operator, keys) in condition {
            for (key, expected_values) in keys {
                // Key format: "OIDC_ISSUER:groups", "OIDC_ISSUER:sub", etc.
                // We simplify to check against context.claims or context.groups
                let actual_values = if key == "OIDC_ISSUER:groups" {
                    context.groups.clone()
                } else if let Some(claim_name) = key.strip_prefix("OIDC_ISSUER:") {
                    context
                        .claims
                        .get(claim_name)
                        .map(|v| vec![v.clone()])
                        .unwrap_or_default()
                } else {
                    vec![]
                };

                match operator.as_str() {
                    "StringEquals" => {
                        if actual_values.is_empty() || !expected_values.contains(&actual_values[0])
                        {
                            return false;
                        }
                    }
                    "ForAnyValue:StringEquals" => {
                        if !actual_values
                            .iter()
                            .any(|val| expected_values.contains(val))
                        {
                            return false;
                        }
                    }
                    _ => {
                        // Unsupported operator, fail safe
                        return false;
                    }
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_policy_evaluation_basic() {
        let json = serde_json::json!({
            "Roles": [
                {
                    "RoleName": "test-role",
                    "Arn": "arn:dfs:iam:::role/test-role",
                    "AssumeRolePolicyDocument": { "Statement": [] },
                    "Policies": [
                        {
                            "PolicyName": "p1",
                            "PolicyDocument": {
                                "Statement": [
                                    {
                                        "Effect": "Allow",
                                        "Action": ["s3:GetObject"],
                                        "Resource": ["arn:dfs:s3:::bucket1/*"]
                                    },
                                    {
                                        "Effect": "Deny",
                                        "Action": ["s3:GetObject"],
                                        "Resource": ["arn:dfs:s3:::bucket1/secret"]
                                    }
                                ]
                            }
                        }
                    ]
                }
            ]
        });
        let config: IamConfig = serde_json::from_value(json).unwrap();
        let eval = PolicyEvaluator::new(config);
        let ctx = EvaluationContext::default();

        assert!(eval.evaluate(
            "s3:GetObject",
            "arn:dfs:s3:::bucket1/file",
            "arn:dfs:iam:::role/test-role",
            &ctx
        ));
        assert!(!eval.evaluate(
            "s3:GetObject",
            "arn:dfs:s3:::bucket1/secret",
            "arn:dfs:iam:::role/test-role",
            &ctx
        ));
        assert!(!eval.evaluate(
            "s3:PutObject",
            "arn:dfs:s3:::bucket1/file",
            "arn:dfs:iam:::role/test-role",
            &ctx
        ));
    }

    #[test]
    fn test_trust_policy_condition() {
        let json = serde_json::json!({
            "Roles": [
                {
                    "RoleName": "role1",
                    "Arn": "arn:dfs:iam:::role/role1",
                    "AssumeRolePolicyDocument": {
                        "Statement": [
                            {
                                "Effect": "Allow",
                                "Action": "sts:AssumeRoleWithWebIdentity",
                                "Condition": {
                                    "ForAnyValue:StringEquals": {
                                        "OIDC_ISSUER:groups": ["admin"]
                                    }
                                }
                            }
                        ]
                    },
                    "Policies": []
                }
            ]
        });
        let config: IamConfig = serde_json::from_value(json).unwrap();
        let eval = PolicyEvaluator::new(config);

        let ctx_admin = EvaluationContext {
            groups: vec!["admin".into(), "user".into()],
            ..Default::default()
        };
        let ctx_user = EvaluationContext {
            groups: vec!["user".into()],
            ..Default::default()
        };

        assert!(eval.can_assume_role("arn:dfs:iam:::role/role1", &ctx_admin));
        assert!(!eval.can_assume_role("arn:dfs:iam:::role/role1", &ctx_user));
    }
}
