use anyhow::Result;
use askama::Template;
use futures::task::Spawn;
use futures::TryStreamExt;
use k8s_openapi::api::apps::v1::RollingUpdateDeployment;
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, SecretVolumeSource, Service, ServicePort, ServiceSpec,
};
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use k8s_openapi::{
    api::{
        apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy},
        core::v1::{Container, PodSpec, PodTemplateSpec, Volume, VolumeMount},
    },
    apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference},
};
use kube::runtime::reflector::store;
use kube::runtime::reflector::ObjectRef;
use kube::runtime::reflector::{reflector, Store};
use kube::runtime::WatchStreamExt;
use kube::CustomResourceExt;
use kube::{
    api::{Api, ObjectMeta, Patch, PatchParams},
    runtime::{
        controller::{Action, Config, Controller},
        watcher,
    },
    Client, Resource,
};

use std::collections::HashMap;
use std::error::Error;
use std::fmt::Write;
use std::{collections::BTreeMap, sync::Arc};
use tokio::time::Duration;
use tracing::*;

mod error;

#[derive(Clone)]
struct Location {
    hostname: String,
    ip4_address: String,
}

struct ZoneMetadata {
    locations: Vec<Location>,
    origin: String,
}

#[derive(Template)]
#[template(path = "zone.txt")]
struct ZoneTemplate<'a> {
    metadata: &'a Arc<ZoneMetadata>,
}

async fn update_zones(client: Client, reader: &Store<Service>) -> anyhow::Result<()> {
    reader.wait_until_ready().await.unwrap();

    let mut zones = HashMap::new();

    for svc in reader.state() {
        match &svc.meta().annotations {
            None => continue,
            Some(annotations) => {
                match annotations.get("external-dns.alpha.kubernetes.io/hostname") {
                    Some(hostname) => match svc.status.as_ref() {
                        None => {
                            info!("{hostname}: has no IP (no status)")
                        }
                        Some(status) => match status.load_balancer.as_ref() {
                            None => {
                                info!("{hostname}: has no IP (no status.loadBalancer)")
                            }
                            Some(load_balancer) => match load_balancer.ingress.as_ref() {
                                None => {
                                    info!("{hostname}: has no IP (no status.loadBalancer.ingress)")
                                }
                                Some(ingress) => {
                                    let (hostname, origin) = hostname.split_once(".").unwrap();
                                    for address in ingress {
                                        if let Some(ip) = address.ip.as_ref() {
                                            let location = Location {
                                                hostname: hostname.to_string(),
                                                ip4_address: ip.to_string(),
                                            };
                                            let zone =
                                                zones.entry(origin.to_string()).or_insert(vec![]);
                                            zone.push(location);
                                        }
                                    }
                                }
                            },
                        },
                    },
                    None => continue,
                }
            }
        }
    }

    let mut data = BTreeMap::new();

    for (origin, locations) in zones.iter() {
        let template = ZoneTemplate {
            metadata: &Arc::new(ZoneMetadata {
                locations: locations.clone(),
                origin: origin.clone(),
            }),
        };
        let rendered = template.render()?;
        data.insert(format!("db.{origin}"), rendered);
    }

    let zones = ConfigMap {
        metadata: ObjectMeta {
            name: Some("zones".to_string()),
            namespace: Some("default".to_string()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    let api = Api::<ConfigMap>::namespaced(client.clone(), "default");
    api.patch("zones", &PatchParams::apply("dnskit").force(), &Patch::Apply(zones)).await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = Client::try_default().await?;

    let api = Api::<Service>::all(client.clone());
    let (reader, writer) = store::<Service>();

    tokio::spawn(async move {
        loop {
            match update_zones(client.clone(), &reader).await {
                Ok(_) => {}
                Err(e) => {
                    info!("Error whilst updating zone data: {}", e);
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });

    let stream = watcher(api, watcher::Config::default().any_semantic())
        .default_backoff()
        .reflect(writer)
        .touched_objects();

    futures::pin_mut!(stream);

    while (stream.try_next().await?).is_some() {}

    Ok(())
}
