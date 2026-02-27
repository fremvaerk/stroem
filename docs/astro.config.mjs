import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";

export default defineConfig({
  site: "https://fremvaerk.github.io",
  base: "/stroem",
  integrations: [
    starlight({
      title: "Strøm",
      description: "Workflow and task orchestration platform",
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/fremvaerk/stroem",
        },
      ],
      editLink: {
        baseUrl: "https://github.com/fremvaerk/stroem/edit/main/docs/",
      },
      customCss: ["./src/styles/custom.css"],
      sidebar: [
        {
          label: "Getting Started",
          items: [
            { label: "Installation", slug: "getting-started/installation" },
            { label: "Quickstart", slug: "getting-started/quickstart" },
            { label: "Configuration", slug: "getting-started/configuration" },
          ],
        },
        {
          label: "Guides",
          items: [
            { label: "Workflow Basics", slug: "guides/workflow-basics" },
            { label: "Action Types", slug: "guides/action-types" },
            { label: "Runners", slug: "guides/runners" },
            { label: "Templating", slug: "guides/templating" },
            { label: "Input & Output", slug: "guides/input-and-output" },
            { label: "Triggers", slug: "guides/triggers" },
            { label: "Hooks", slug: "guides/hooks" },
            { label: "Connections", slug: "guides/connections" },
            {
              label: "Secrets & Encryption",
              slug: "guides/secrets",
            },
            { label: "Multi-Workspace", slug: "guides/multi-workspace" },
          ],
        },
        {
          label: "Operations",
          collapsed: true,
          items: [
            { label: "Authentication", slug: "operations/authentication" },
            { label: "Log Storage", slug: "operations/log-storage" },
            { label: "Recovery", slug: "operations/recovery" },
            { label: "Startup Scripts", slug: "operations/startup-scripts" },
          ],
        },
        {
          label: "Deployment",
          collapsed: true,
          items: [
            { label: "Docker Compose", slug: "deployment/docker-compose" },
            {
              label: "Helm / Kubernetes",
              slug: "deployment/helm",
            },
          ],
        },
        {
          label: "Reference",
          collapsed: true,
          items: [
            { label: "CLI", slug: "reference/cli" },
            { label: "API", slug: "reference/api" },
            { label: "Webhook API", slug: "reference/webhook-api" },
            { label: "Auth API", slug: "reference/auth-api" },
            { label: "Worker API", slug: "reference/worker-api" },
          ],
        },
        {
          label: "Examples",
          collapsed: true,
          items: [
            { label: "Hello World", slug: "examples/hello-world" },
            { label: "Deploy Pipeline", slug: "examples/deploy-pipeline" },
            { label: "CI Pipeline", slug: "examples/ci-pipeline" },
          ],
        },
      ],
    }),
  ],
});
