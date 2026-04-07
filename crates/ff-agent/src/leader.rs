use ff_core::{ActivityLevel, AgentRegistrationAck, AgentTask, NodeRole, TaskResult};
use ff_discovery::{HardwareProfile, HealthSnapshot};
use reqwest::StatusCode;
use serde::Serialize;

#[derive(Clone)]
pub struct LeaderClient {
    http: reqwest::Client,
    leader_url: String,
    node_id: String,
}

impl LeaderClient {
    pub fn new(leader_url: String, node_id: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            leader_url,
            node_id,
        }
    }

    pub async fn register(&self, hardware: &HardwareProfile) -> AgentRegistrationAck {
        #[derive(Serialize)]
        struct RegisterPayload<'a> {
            node_id: &'a str,
            hostname: &'a str,
            hardware: &'a HardwareProfile,
        }

        let payload = RegisterPayload {
            node_id: &self.node_id,
            hostname: &hardware.hostname,
            hardware,
        };

        for endpoint in ["/agent/register", "/register", "/fleet/register"] {
            let url = format!("{}{}", self.leader_url, endpoint);
            if let Ok(resp) = self.http.post(&url).json(&payload).send().await
                && resp.status().is_success()
                && let Ok(ack) = resp.json::<AgentRegistrationAck>().await
            {
                return ack;
            }
        }

        AgentRegistrationAck::default()
    }

    pub async fn send_heartbeat(
        &self,
        role: NodeRole,
        activity_level: ActivityLevel,
        health: &HealthSnapshot,
    ) -> anyhow::Result<()> {
        #[derive(Serialize)]
        struct HeartbeatPayload<'a> {
            node_id: &'a str,
            role: NodeRole,
            activity_level: ActivityLevel,
            health: &'a HealthSnapshot,
        }

        let payload = HeartbeatPayload {
            node_id: &self.node_id,
            role,
            activity_level,
            health,
        };

        self.post_best_effort(
            &["/agent/heartbeat", "/heartbeat", "/fleet/heartbeat"],
            &payload,
        )
        .await
    }

    pub async fn fetch_task(&self) -> anyhow::Result<Option<AgentTask>> {
        for endpoint in ["/agent/tasks/next", "/tasks/next", "/fleet/tasks/next"] {
            let url = format!("{}{}?node_id={}", self.leader_url, endpoint, self.node_id);
            match self.http.get(&url).send().await {
                Ok(resp) if resp.status() == StatusCode::NO_CONTENT => return Ok(None),
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(task) = resp.json::<AgentTask>().await {
                        return Ok(Some(task));
                    }
                }
                Ok(_) => continue,
                Err(_) => continue,
            }
        }

        Ok(None)
    }

    pub async fn report_task_result(&self, result: &TaskResult) -> anyhow::Result<()> {
        self.post_best_effort(
            &[
                "/agent/tasks/result",
                "/tasks/result",
                "/fleet/tasks/result",
            ],
            result,
        )
        .await
    }

    async fn post_best_effort<T: Serialize + ?Sized>(
        &self,
        endpoints: &[&str],
        payload: &T,
    ) -> anyhow::Result<()> {
        let mut last_err: Option<anyhow::Error> = None;

        for endpoint in endpoints {
            let url = format!("{}{}", self.leader_url, endpoint);
            match self.http.post(&url).json(payload).send().await {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                Ok(resp) => {
                    last_err = Some(anyhow::anyhow!(
                        "leader status {} from {}",
                        resp.status(),
                        url
                    ));
                }
                Err(err) => {
                    last_err = Some(anyhow::Error::new(err));
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no reachable leader endpoint")))
    }
}
