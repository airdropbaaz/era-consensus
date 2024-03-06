use crate::{config, NodeAddr};
use anyhow::{anyhow, Context};
use k8s_openapi::{
    api::{
        apps::v1::{Deployment, DeploymentSpec},
        core::v1::{
            Container, ContainerPort, EnvVar, EnvVarSource, HTTPGetAction, Namespace,
            ObjectFieldSelector, Pod, PodSpec, PodTemplateSpec, Probe,
        },
    },
    apimachinery::pkg::{apis::meta::v1::LabelSelector, util::intstr::IntOrString::Int},
};
use kube::{
    api::{ListParams, PostParams},
    core::{ObjectList, ObjectMeta},
    Api, Client, ResourceExt,
};
use std::collections::{BTreeMap, HashMap};
use tokio_retry::strategy::FixedInterval;
use tokio_retry::Retry;
use tracing::log::info;
use zksync_protobuf::serde::Serde;

/// Get a kube client
pub async fn get_client() -> anyhow::Result<Client> {
    Ok(Client::try_default().await?)
}

/// Creates a namespace in k8s cluster
pub async fn create_or_reuse_namespace(client: &Client, name: &str) -> anyhow::Result<()> {
    let namespaces: Api<Namespace> = Api::all(client.clone());
    match namespaces.get_opt(name).await? {
        None => {
            let namespace = Namespace {
                metadata: ObjectMeta {
                    name: Some(name.to_owned()),
                    labels: Some(BTreeMap::from([("name".to_owned(), name.to_owned())])),
                    ..Default::default()
                },
                ..Default::default()
            };

            let namespaces: Api<Namespace> = Api::all(client.clone());
            let post_params = PostParams::default();
            let result = namespaces.create(&post_params, &namespace).await?;

            info!(
                "Namespace: {} ,created",
                result
                    .metadata
                    .name
                    .context("Name not defined in metadata")?
            );
            Ok(())
        }
        Some(consensus_namespace) => {
            info!(
                "Namespace: {} ,already exists",
                consensus_namespace
                    .metadata
                    .name
                    .context("Name not defined in metadata")?
            );
            Ok(())
        }
    }
}

/// Creates a deployment
pub async fn deploy_node(
    client: &Client,
    node_index: usize,
    is_seed: bool,
    peers: Vec<NodeAddr>,
    namespace: &str,
) -> anyhow::Result<()> {
    let cli_args = get_cli_args(peers);
    let node_name = format!("consensus-node-{node_index:0>2}");
    let deployment = Deployment {
        metadata: ObjectMeta {
            name: Some(node_name.to_owned()),
            namespace: Some(namespace.to_owned()),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            selector: LabelSelector {
                match_labels: Some(BTreeMap::from([("app".to_owned(), node_name.to_owned())])),
                ..Default::default()
            },
            replicas: Some(1),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(BTreeMap::from([
                        ("app".to_owned(), node_name.to_owned()),
                        ("id".to_owned(), node_name.to_owned()),
                        ("seed".to_owned(), is_seed.to_string()),
                    ])),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    containers: vec![Container {
                        name: node_name.to_owned(),
                        image: Some("consensus-node".to_owned()),
                        env: Some(vec![
                            EnvVar {
                                name: "NODE_ID".to_owned(),
                                value: Some(node_name.to_owned()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "PUBLIC_ADDR".to_owned(),
                                value_from: Some(EnvVarSource {
                                    field_ref: Some(ObjectFieldSelector {
                                        field_path: "status.podIP".to_owned(),
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            },
                        ]),
                        command: Some(vec!["./k8s_entrypoint.sh".to_owned()]),
                        args: Some(cli_args),
                        image_pull_policy: Some("Never".to_owned()),
                        ports: Some(vec![
                            ContainerPort {
                                container_port: i32::from(config::NODES_PORT),
                                ..Default::default()
                            },
                            ContainerPort {
                                container_port: 3154,
                                ..Default::default()
                            },
                        ]),
                        liveness_probe: Some(Probe {
                            http_get: Some(HTTPGetAction {
                                path: Some("/health".to_owned()),
                                port: Int(3154),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        readiness_probe: Some(Probe {
                            http_get: Some(HTTPGetAction {
                                path: Some("/health".to_owned()),
                                port: Int(3154),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    let deployments: Api<Deployment> = Api::namespaced(client.clone(), namespace);
    let post_params = PostParams::default();
    let result = deployments.create(&post_params, &deployment).await?;

    info!(
        "Deployment: {} , created",
        result
            .metadata
            .name
            .context("Name not defined in metadata")?
    );
    Ok(())
}

/// Returns a HashMap with mapping: node_id -> IP address
pub async fn get_seed_node_addrs(
    client: &Client,
    amount: usize,
    namespace: &str,
) -> anyhow::Result<HashMap<String, String>> {
    let mut seed_nodes = HashMap::new();
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);

    // Will retry 15 times during 15 seconds to allow pods to start and obtain an IP
    let retry_strategy = FixedInterval::from_millis(1000).take(15);
    let pod_list = Retry::spawn(retry_strategy, || get_seed_pods(&pods, amount)).await?;

    for p in pod_list {
        let node_id = p.labels()["id"].clone();
        seed_nodes.insert(
            node_id,
            p.status
                .context("Status not present")?
                .pod_ip
                .context("Pod IP address not present")?,
        );
    }
    Ok(seed_nodes)
}

async fn get_seed_pods(pods: &Api<Pod>, amount: usize) -> anyhow::Result<ObjectList<Pod>> {
    let lp = ListParams::default().labels("seed=true");
    let p = pods.list(&lp).await?;
    if p.items.len() == amount && p.iter().all(is_pod_running) {
        Ok(p)
    } else {
        Err(anyhow!("Pods are not ready"))
    }
}

fn is_pod_running(pod: &Pod) -> bool {
    if let Some(status) = &pod.status {
        if let Some(phase) = &status.phase {
            return phase == "Running";
        }
    }
    false
}

fn get_cli_args(peers: Vec<NodeAddr>) -> Vec<String> {
    if peers.is_empty() {
        [].to_vec()
    } else {
        [
            "--add-gossip-static-outbound".to_string(),
            config::encode_with_serializer(
                &peers
                    .iter()
                    .map(|e| Serde(e.clone()))
                    .collect::<Vec<Serde<NodeAddr>>>(),
                serde_json::Serializer::new(vec![]),
            ),
        ]
        .to_vec()
    }
}