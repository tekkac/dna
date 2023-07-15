use std::{sync::Arc, time::Duration};

use futures::Stream;
use k8s_openapi::{
    api,
    apimachinery::pkg::{
        apis::meta::{self, v1::Condition},
        util::intstr::IntOrString,
    },
    chrono::{DateTime, Utc},
};
use kube::{
    api::{DeleteParams, ListParams, Patch, PatchParams},
    runtime::{controller::Action, finalizer, watcher::Config, Controller},
    Api, CustomResourceExt, Resource, ResourceExt,
};
use serde_json::json;
use tracing::{error, info, instrument, warn};

use super::{
    common::CommonStatus,
    webhook::{SinkWebhook, SinkWebhookStatus},
};
use crate::reconcile::{Context, Error, ReconcileItem};

static WEBHOOK_FINALIZER: &str = "sinkwebhook.apibara.com";

impl SinkWebhook {
    #[instrument(skip_all)]
    async fn reconcile(&self, ctx: Arc<Context>) -> Result<Action, Error> {
        use api::core::v1::Pod;

        let ns = self.namespace().expect("webhook is namespaced");
        let name = self.name_any();

        let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
        let webhooks: Api<SinkWebhook> = Api::namespaced(ctx.client.clone(), &ns);

        // Check if there is a pod associated with this sink.
        let existing_pod = if let Some(pod_name) = self
            .status
            .as_ref()
            .and_then(|status| status.common.instance_name.as_ref())
        {
            pods.get_opt(pod_name).await?
        } else {
            None
        };

        let mut restart_increment = 0;
        let mut error_condition = None;
        if let Some(existing_pod) = existing_pod {
            // The pod exists. Is it running?
            let container_status = existing_pod.status.as_ref().and_then(|status| {
                status
                    .container_statuses
                    .as_ref()
                    .and_then(|statuses| statuses.first())
            });

            let container_finished_at = container_status
                .and_then(|cs| cs.state.as_ref())
                .and_then(|st| st.terminated.clone())
                .and_then(|ts| ts.finished_at);

            // Delete pod so that the next section will recreate a new one.
            // For now, delete once every minute.
            // TODO: should depend on the exit code.
            if let Some(finished_at) = container_finished_at {
                let elapsed = (Utc::now().time() - finished_at.0.time())
                    .to_std()
                    .unwrap_or_default();

                if elapsed > Duration::from_secs(60) {
                    info!(pod = %existing_pod.name_any(), "deleting pod");
                    pods.delete(&existing_pod.name_any(), &DeleteParams::default())
                        .await?;
                    restart_increment = 1;
                } else {
                    error_condition = Some(Condition {
                        last_transition_time: finished_at,
                        type_: "PodTerminated".to_string(),
                        message: "Pod has been terminated".to_string(),
                        observed_generation: self.meta().generation,
                        reason: "PodTerminate".to_string(),
                        status: "False".to_string(),
                    });
                }
            }
        }

        let metadata = self.object_metadata(&ctx);
        let spec = self.pod_spec(&ctx);
        let pod_manifest = Pod {
            metadata,
            spec: Some(spec),
            ..Pod::default()
        };

        let pod = pods
            .patch(
                &name,
                &PatchParams::apply("sinkwebhook"),
                &Patch::Apply(pod_manifest),
            )
            .await?;

        let pod_scheduled_condition = Condition {
            last_transition_time: pod
                .meta()
                .creation_timestamp
                .clone()
                .unwrap_or(meta::v1::Time(DateTime::<Utc>::MIN_UTC)),
            type_: "PodScheduled".to_string(),
            message: "Pod has been scheduled".to_string(),
            observed_generation: self.meta().generation,
            reason: "PodScheduled".to_string(),
            status: "True".to_string(),
        };

        let phase = if error_condition.is_some() {
            "Error".to_string()
        } else {
            "Running".to_string()
        };

        let mut conditions = vec![pod_scheduled_condition];
        if let Some(condition) = error_condition {
            conditions.push(condition);
        }

        let restart_count = self
            .status
            .as_ref()
            .map(|status| status.common.restart_count.unwrap_or_default() + restart_increment);

        let status = json!({
            "status": SinkWebhookStatus {
                common: CommonStatus {
                    pod_created: pod.meta().creation_timestamp.clone(),
                    instance_name: pod.meta().name.clone(),
                    phase: Some(phase),
                    conditions: Some(conditions),
                    restart_count,
                }
            }
        });

        webhooks
            .patch_status(&name, &PatchParams::default(), &Patch::Merge(&status))
            .await?;

        Ok(Action::requeue(Duration::from_secs(10)))
    }

    #[instrument(skip_all)]
    async fn cleanup(&self, ctx: Arc<Context>) -> Result<Action, Error> {
        use api::core::v1::Pod;

        let ns = self.namespace().expect("webhook is namespaced");
        let name = self.name_any();
        let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);

        if let Some(_existing) = pods.get_opt(&name).await? {
            pods.delete(&name, &DeleteParams::default()).await?;
        }

        Ok(Action::requeue(Duration::from_secs(10)))
    }

    fn object_metadata(&self, _ctx: &Arc<Context>) -> meta::v1::ObjectMeta {
        use meta::v1::ObjectMeta;

        ObjectMeta {
            name: self.metadata.name.clone(),
            ..ObjectMeta::default()
        }
    }

    fn pod_spec(&self, ctx: &Arc<Context>) -> api::core::v1::PodSpec {
        use api::core::v1::{Container, ContainerPort, EnvVar, HTTPGetAction, PodSpec, Probe};

        let image = self
            .spec
            .common
            .image
            .as_ref()
            .and_then(|image| image.name.clone())
            .unwrap_or_else(|| ctx.configuration.webhook.image.clone());

        let image_pull_secrets = self
            .spec
            .common
            .image
            .as_ref()
            .and_then(|image| image.pull_secrets.clone());
        let image_pull_policy = self
            .spec
            .common
            .image
            .as_ref()
            .and_then(|image| image.pull_policy.clone());

        let probe = Probe {
            http_get: Some(HTTPGetAction {
                path: Some("/status".to_string()),
                port: IntOrString::Int(8118),
                scheme: Some("HTTP".to_string()),
                ..HTTPGetAction::default()
            }),
            ..Probe::default()
        };

        let args = vec!["--status-server-address=0.0.0.0:8118".to_string()];

        let mut volumes = vec![];
        let mut volume_mounts = vec![];
        let mut env = vec![EnvVar {
            name: "TARGET_URL".to_string(),
            value: Some(self.spec.target_url.clone()),
            ..EnvVar::default()
        }];

        if self.spec.raw.unwrap_or(false) {
            env.push(EnvVar {
                name: "RAW".to_string(),
                value: Some("true".to_string()),
                ..EnvVar::default()
            });
        }

        // TODO: add headers environment variable, like METADATA

        env.extend(self.spec.common.to_env_var());

        if let Some((filter_volume, filter_mount, filter_env)) =
            self.spec.common.stream.filter_data()
        {
            volumes.push(filter_volume);
            volume_mounts.push(filter_mount);
            env.push(filter_env);
        }

        if let Some((transform_volume, transform_mount, transform_env)) =
            self.spec.common.stream.transform_data()
        {
            volumes.push(transform_volume);
            volume_mounts.push(transform_mount);
            env.push(transform_env);
        }

        let container = Container {
            name: "sink".to_string(),
            image: Some(image),
            args: Some(args),
            env: Some(env),
            ports: Some(vec![ContainerPort {
                container_port: 8118,
                name: Some("status".to_string()),
                ..ContainerPort::default()
            }]),
            image_pull_policy,
            liveness_probe: Some(probe.clone()),
            readiness_probe: Some(probe),
            volume_mounts: Some(volume_mounts),
            ..Container::default()
        };

        PodSpec {
            containers: vec![container],
            volumes: Some(volumes),
            image_pull_secrets,
            restart_policy: Some("Never".to_string()),
            ..PodSpec::default()
        }
    }
}

async fn reconcile_webhook(webhook: Arc<SinkWebhook>, ctx: Arc<Context>) -> Result<Action, Error> {
    let ns = webhook.namespace().expect("webhook is namespaced");
    let webhooks: Api<SinkWebhook> = Api::namespaced(ctx.client.clone(), &ns);

    info!(
        webhook = %webhook.name_any(),
        namespace = %ns,
        "reconcile webhook sink",
    );

    finalizer::finalizer(&webhooks, WEBHOOK_FINALIZER, webhook, |event| async {
        use finalizer::Event::*;
        match event {
            Apply(webhook) => webhook.reconcile(ctx.clone()).await,
            Cleanup(webhook) => webhook.cleanup(ctx.clone()).await,
        }
    })
    .await
    .map_err(|err| Error::Finalizer(err.into()))
}

fn error_policy(_webhook: Arc<SinkWebhook>, error: &Error, _ctx: Arc<Context>) -> Action {
    warn!(error = ?error, "webhook reconcile error");
    Action::requeue(Duration::from_secs(30))
}

pub async fn start_controller(
    ctx: Context,
) -> Result<impl Stream<Item = ReconcileItem<SinkWebhook>>, Error> {
    let webhooks = Api::<SinkWebhook>::all(ctx.client.clone());

    if webhooks.list(&ListParams::default()).await.is_err() {
        error!("WebhookSink CRD not installed");
        return Err(Error::CrdNotInstalled(SinkWebhook::crd_name().to_string()));
    }

    info!("starting webhook sink controller");

    let pods = Api::<api::core::v1::Pod>::all(ctx.client.clone());
    let controller = Controller::new(webhooks, Config::default())
        .owns(pods, Config::default())
        .run(reconcile_webhook, error_policy, ctx.into());

    Ok(controller)
}
