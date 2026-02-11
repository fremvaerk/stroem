## Problem

In this days average company uses a lot of different tools for running tasks:

* RunDeck for tasks running on schedule - daily, weekly, monthly
* WindMill.dev as an attempt to move away from RunDeck
* CirleCI for building on github commits
* GitHub Actions for building and deploying
* Kubernetes CronJobs for periodics jobs

The idea of this project is to cover all the cases, provided by mentioned tools, having only one tool instead.

## Proposed tech stack

Backend: Rust
Frontend: React, https://ui.shadcn.com/

## Proposal - to be discussed - ideas in random order

It should follow GitOps principles - the definition of the workflows/pipelines should be in git (yaml, probably)), and the service should fetch the updates.

Also it would be great to include a library of actions from another git repo and use it.

The minimal brick is "Action", which can be implemented in any language (bash, python, TS, or binary). It should have defined input parameters and output data.

Next level is a Workflow (or Pipeline), which is basically a DAG of the Actions, defining, how the data is passed between them. It also should have input and output data defined.
The Workflow/Pipeline can be run manually (using UI or API), and have number of triggers - on URL (webhooks from github as example), different queries (RabbitMQ, AWS Kinesis etc), database updates (Postgresql etc)

UI for running the Workflow should be generated on the fly, using input data definitions.

Authorisation using OIDC for users. Users can have API keys for calling API.

The main purpose of UI is to monitor the schedulers, run the Workflows, viewing logs, including during the task execution (use WebSocket)

Logs of the executions should be stored in S3 bucket

The service should consist from following parts:

* Server - which serves UI, checks the shcedule, keeps the queue for tasks - sounds like a bottle neck, probably should be rethinked
* Worker - reads the queue and gets the tasks from there, can start N runners.
* Runner - which actually runs the task. Runner can be local (so it runs on the same node as Worker), docker, k8s pod.

Docker-in-Docker should be used (so it is possible to build Docker images, as example)

Another example - the tool should be able to run Ansible, Terraform etc.

In the workflow it should be possible output of one action put into input of another action - there should be some tempating used.

Secrets - TBD. As on option - SOPS files. Should be possible to extend and use, as example, AWS Secrets Manager or GCP Secrets etc.
check https://github.com/helmfile/vals
https://github.com/getsops/sops

Initially I would like to have a helm chart for running in K8S cluster.


Everything here is to be discussed.
