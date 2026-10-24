//! Observe readiness after Helm completes, so admission failures can terminate
//! promptly without killing an in-flight Helm mutation or leaving a pending lock.
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::install_events::{self, Operation, ProgressKind};

pub const POD_SECURITY_REJECTED: &str = "Kubernetes Pod Security rejected the workload. Check the chart's namespace policy and pod security requirements; waiting longer will not resolve this admission failure.";
pub const POD_CREATION_FAILED: &str = "Kubernetes could not create the workload's Pods. Check namespace events for admission, quota, or service-account errors.";
pub const ROLLOUT_FAILED: &str = "The workload exceeded its rollout progress deadline. Check its Pods, image pulls, and volume events.";
pub const ROLLOUT_TIMED_OUT: &str = "Timed out waiting for workload readiness. Check its Pods, image pulls, and volume events before retrying.";

fn deployment_ready(deployment: &Value) -> Result<bool, &'static str> {
    let generation = deployment.pointer("/metadata/generation").and_then(Value::as_u64);
    let observed = deployment.pointer("/status/observedGeneration").and_then(Value::as_u64);
    if !matches!((generation, observed), (Some(current), Some(seen)) if seen >= current) {
        return Ok(false);
    }
    for condition in deployment.pointer("/status/conditions").and_then(Value::as_array).into_iter().flatten() {
        if condition["type"] == "ReplicaFailure" && condition["status"] == "True" {
            return Err(if condition["message"].as_str().is_some_and(|text| text.contains("violates PodSecurity")) {
                POD_SECURITY_REJECTED
            } else {
                POD_CREATION_FAILED
            });
        }
        if condition["type"] == "Progressing" && condition["status"] == "False"
            && condition["reason"] == "ProgressDeadlineExceeded" {
            return Err(ROLLOUT_FAILED);
        }
    }
    let desired = deployment.pointer("/spec/replicas").and_then(Value::as_u64).unwrap_or(1);
    Ok(desired > 0 && ["replicas", "updatedReplicas", "readyReplicas", "availableReplicas"]
        .iter().all(|field| deployment["status"][*field].as_u64() == Some(desired)))
}

pub fn wait_for_deployment(namespace: &str, name: &str, operation: Operation) -> Result<(), String> {
    let began = Instant::now();
    let deadline = began + Duration::from_secs(900);
    let cancelled = AtomicBool::new(false);
    let mut heartbeat = began;
    install_events::progress(operation, ProgressKind::Start, Duration::ZERO);
    while Instant::now() < deadline {
        let response = crate::checks::probe("kubectl", &["get", "deployment", name, "-n", namespace, "-o", "json"], &cancelled, deadline);
        if let Some(deployment) = response.filter(|result| result.success)
            .and_then(|result| serde_json::from_str::<Value>(&result.output).ok()) {
            match deployment_ready(&deployment) {
                Ok(true) => {
                    install_events::progress(operation, ProgressKind::Success(None), began.elapsed());
                    return Ok(());
                }
                Err(message) => {
                    install_events::progress(operation, ProgressKind::Failure(None), began.elapsed());
                    return Err(message.into());
                }
                Ok(false) => {}
            }
        }
        if heartbeat.elapsed() >= Duration::from_secs(10) {
            install_events::progress(operation, ProgressKind::Wait, began.elapsed());
            heartbeat = Instant::now();
        }
        std::thread::sleep(Duration::from_secs(2).min(deadline.saturating_duration_since(Instant::now())));
    }
    install_events::progress(operation, ProgressKind::Timeout, began.elapsed());
    Err(ROLLOUT_TIMED_OUT.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ready() -> Value {
        json!({"metadata":{"generation":2}, "spec":{"replicas":1},
            "status":{"observedGeneration":2,"replicas":1,"updatedReplicas":1,"readyReplicas":1,"availableReplicas":1}})
    }

    #[test]
    fn readiness_requires_current_generation_and_available_updated_replicas() {
        assert_eq!(deployment_ready(&ready()), Ok(true));
        for field in ["replicas", "updatedReplicas", "readyReplicas", "availableReplicas"] {
            let mut deployment = ready();
            deployment["status"][field] = json!(0);
            assert_eq!(deployment_ready(&deployment), Ok(false));
        }
        let mut deployment = ready();
        deployment["status"]["observedGeneration"] = json!(1);
        assert_eq!(deployment_ready(&deployment), Ok(false));
        assert_eq!(deployment_ready(&json!({})), Ok(false));
    }

    #[test]
    fn admission_failure_is_actionable_without_reflecting_cluster_text() {
        let mut deployment = ready();
        deployment["status"]["conditions"] = json!([{"type":"ReplicaFailure","status":"True","reason":"FailedCreate","message":"private pod name: violates PodSecurity baseline"}]);
        assert_eq!(deployment_ready(&deployment), Err(POD_SECURITY_REJECTED));
        deployment["status"]["conditions"][0]["message"] = json!("quota exceeded for private namespace");
        assert_eq!(deployment_ready(&deployment), Err(POD_CREATION_FAILED));
        deployment["status"]["conditions"][0]["status"] = json!("False");
        assert_eq!(deployment_ready(&deployment), Ok(true));
    }
}
