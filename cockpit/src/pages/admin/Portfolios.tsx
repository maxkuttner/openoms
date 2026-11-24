import { useState } from "react";
import { Button } from "@/components/ui/button";
import { CrudResource } from "../../components/CrudResource";
import { PortfolioGrants } from "../../components/admin/PortfolioGrants";
import { statusOptions } from "../../lifecycle";

const STATUS = statusOptions("portfolio");

export function PortfoliosPage() {
  const [granting, setGranting] = useState<{ id: string; code: string } | null>(null);
  return (
    <>
      <CrudResource
        title="Portfolios"
        path="/admin/portfolios"
        editable
        lifecycle={{ resource: "portfolio" }}
        rowAction={(row: { id: string; code: string }) => (
          <Button size="xs" variant="secondary" onClick={() => setGranting(row)}>Grants</Button>
        )}
        columns={[
          { key: "code", label: "Code" },
          { key: "name", label: "Name" },
          { key: "status", label: "Status" },
        ]}
        fields={[
          { name: "code", label: "Code", required: true },
          { name: "name", label: "Name", required: true },
          { name: "status", label: "Status", type: "select", required: true, options: STATUS, default: "ACTIVE" },
        ]}
      />
      {granting && <PortfolioGrants portfolio={granting} onClose={() => setGranting(null)} />}
    </>
  );
}
