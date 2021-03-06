use ipis::{core::anyhow::Result, log::info};
use k8s_openapi::api::{
    batch::v1::{CronJob, CronJobSpec, Job, JobSpec, JobTemplateSpec},
    core::v1::{
        ConfigMapKeySelector, ConfigMapVolumeSource, Container, EnvVar, EnvVarSource, KeyToPath,
        PodSpec, PodTemplateSpec, SecretVolumeSource, Volume, VolumeMount,
    },
};
use kube::{
    api::{DeleteParams, ListParams, PostParams},
    core::ObjectMeta,
    Api, Client, Error,
};

use crate::r#box::{BoxAccessSpec, BoxMachineSpec, BoxState};

#[derive(Default)]
pub struct AnsibleClient {}

impl AnsibleClient {
    pub const LABEL_BOX_NAME: &'static str = "kiss.netai-cloud/box_name";
    pub const LABEL_BOX_ACCESS_ADDRESS: &'static str = "kiss.netai-cloud/box_access_address";
    pub const LABEL_BOX_MACHINE_UUID: &'static str = "kiss.netai-cloud/box_machine_uuid";
    pub const LABEL_COMPLETED_STATE: &'static str = "kiss.netai-cloud/completed_state";

    pub async fn spawn(&self, kube: &Client, job: AnsibleJob<'_>) -> Result<(), Error> {
        let ns = "kiss";
        let box_name = job.machine.uuid.to_string();
        let name = format!("box-{}-{}", &job.task, &box_name);

        // delete all previous cronjobs
        {
            let api = Api::<CronJob>::namespaced(kube.clone(), ns);
            let dp = DeleteParams::background();
            let lp = ListParams {
                label_selector: Some(format!("{}={}", AnsibleClient::LABEL_BOX_NAME, &box_name)),
                ..Default::default()
            };
            api.delete_collection(&dp, &lp).await?;
        }
        // delete all previous jobs
        {
            let api = Api::<Job>::namespaced(kube.clone(), ns);
            let dp = DeleteParams::background();
            let lp = ListParams {
                label_selector: Some(format!("{}={}", AnsibleClient::LABEL_BOX_NAME, &box_name)),
                ..Default::default()
            };
            api.delete_collection(&dp, &lp).await?;
        }

        // define the object
        let metadata = ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(ns.into()),
            labels: Some(
                vec![
                    Some((Self::LABEL_BOX_NAME.into(), box_name.clone())),
                    Some((
                        Self::LABEL_BOX_ACCESS_ADDRESS.into(),
                        job.access.address.to_string(),
                    )),
                    Some((
                        Self::LABEL_BOX_MACHINE_UUID.into(),
                        job.machine.uuid.to_string(),
                    )),
                    job.completed_state
                        .as_ref()
                        .map(ToString::to_string)
                        .map(|state| (Self::LABEL_COMPLETED_STATE.into(), state)),
                ]
                .into_iter()
                .flatten()
                .collect(),
            ),
            ..Default::default()
        };
        let spec = JobSpec {
            template: PodTemplateSpec {
                spec: Some(PodSpec {
                    restart_policy: Some("OnFailure".into()),
                    service_account: Some("ansible-playbook".into()),
                    containers: vec![Container {
                        name: "ansible".into(),
                        image: Some("cytopia/ansible:latest-awsk8s".into()),
                        command: Some(vec!["ansible-playbook".into()]),
                        args: Some(vec!["-vvv".into(), "/opt/playbook/playbook.yaml".into()]),
                        env: Some(vec![
                            EnvVar {
                                name: "ansible_host".into(),
                                value: Some(job.machine.hostname()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "ansible_host_id".into(),
                                value: Some(box_name.to_string()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "ansible_host_uuid".into(),
                                value: Some(job.machine.uuid.to_string()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "ansible_ssh_host".into(),
                                value: Some(job.access.address.to_string()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "ansible_user".into(),
                                value_from: Some(EnvVarSource {
                                    config_map_key_ref: Some(ConfigMapKeySelector {
                                        name: Some("matchbox-account".into()),
                                        key: "username".into(),
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "ansible_ssh_private_key_file".into(),
                                value: Some("/root/.ssh/id_rsa".into()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "ansible_ipmi_password".into(),
                                value: Some("kiss".into()),
                                ..Default::default()
                            },
                        ]),
                        volume_mounts: Some(vec![
                            VolumeMount {
                                name: "playbook".into(),
                                mount_path: "/opt/playbook".into(),
                                ..Default::default()
                            },
                            VolumeMount {
                                name: "tasks".into(),
                                mount_path: "/opt/playbook/tasks".into(),
                                ..Default::default()
                            },
                            VolumeMount {
                                name: "ssh".into(),
                                mount_path: "/root/.ssh".into(),
                                ..Default::default()
                            },
                        ]),
                        ..Default::default()
                    }],
                    volumes: Some(vec![
                        Volume {
                            name: "playbook".into(),
                            config_map: Some(ConfigMapVolumeSource {
                                name: Some("ansible-playbook".into()),
                                default_mode: Some(0o400),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        Volume {
                            name: "tasks".into(),
                            config_map: Some(ConfigMapVolumeSource {
                                name: Some(format!("ansible-task-{}", &job.task)),
                                default_mode: Some(0o400),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        Volume {
                            name: "ssh".into(),
                            secret: Some(SecretVolumeSource {
                                secret_name: Some("matchbox-account".into()),
                                default_mode: Some(0o400),
                                items: Some(vec![KeyToPath {
                                    key: "id_rsa".into(),
                                    path: "id_rsa".into(),
                                    ..Default::default()
                                }]),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let pp = PostParams {
            dry_run: false,
            field_manager: Some("kube-controller".into()),
        };

        match job.cron {
            Some(schedule) => {
                let api = Api::<CronJob>::namespaced(kube.clone(), ns);
                let job = CronJob {
                    metadata: metadata.clone(),
                    spec: Some(CronJobSpec {
                        concurrency_policy: Some("Forbid".into()),
                        schedule: schedule.into(),
                        job_template: JobTemplateSpec {
                            metadata: Some(metadata),
                            spec: Some(spec),
                        },
                        ..Default::default()
                    }),
                    status: None,
                };
                api.create(&pp, &job).await?;
            }
            None => {
                let api = Api::<Job>::namespaced(kube.clone(), ns);
                let job = Job {
                    metadata,
                    spec: Some(spec),
                    status: None,
                };
                api.create(&pp, &job).await?;
            }
        }

        info!("spawned a job: {name}");
        Ok(())
    }
}

pub struct AnsibleJob<'a> {
    pub cron: Option<&'static str>,
    pub task: &'static str,
    pub access: &'a BoxAccessSpec,
    pub machine: &'a BoxMachineSpec,
    pub completed_state: Option<BoxState>,
}
