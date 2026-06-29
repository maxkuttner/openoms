import { ApiReferenceReact } from "@scalar/api-reference-react";
import "@scalar/api-reference-react/style.css";

// Embedded Scalar API reference, fed from the OMS OpenAPI doc through the dev proxy
// (/api → OMS; the spec lives at /api-docs/openapi.json on the server).
export function ApiDocsPage() {
  return (
    <div style={{ margin: "calc(-1 * var(--mantine-spacing-md))" }}>
      <ApiReferenceReact
        configuration={{
          spec: { url: "/api/api-docs/openapi.json" },
          darkMode: true,
          hideModels: false,
          hideDownloadButton: false,
        }}
      />
    </div>
  );
}
