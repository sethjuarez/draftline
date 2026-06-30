import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import mermaid from "astro-mermaid";

export default defineConfig({
  site: "https://draftline.dev",
  output: "static",
  integrations: [
    mermaid({
      autoTheme: true,
      enableLog: false,
      mermaidConfig: {
        securityLevel: "strict",
        flowchart: {
          curve: "basis",
        },
      },
    }),
    starlight({
      title: "Draftline",
      logo: {
        src: "./src/assets/draftline-mark.svg",
      },
      customCss: ["./src/styles/starlight.css"],
      head: [
        {
          tag: "link",
          attrs: {
            rel: "stylesheet",
            href: "/diagram-lightbox.css",
          },
        },
        {
          tag: "script",
          attrs: {
            src: "/diagram-lightbox.js",
            defer: true,
          },
        },
      ],
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/sethjuarez/draftline",
        },
      ],
      sidebar: [
        {
          label: "Start here",
          items: [
            { label: "Overview", slug: "docs" },
            { label: "Scenario map", slug: "docs/scenarios" },
          ],
        },
        {
          label: "Scenario docs",
          items: [
            { label: "Workspace setup", slug: "docs/scenarios/workspace" },
            { label: "Content policy", slug: "docs/scenarios/content-policy" },
            { label: "Authoring and versions", slug: "docs/scenarios/authoring" },
            { label: "Collaboration", slug: "docs/scenarios/collaboration" },
            { label: "Recovery and cleanup", slug: "docs/scenarios/recovery-cleanup" },
          ],
        },
      ],
    }),
  ],
});
