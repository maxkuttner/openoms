import { CrudResource } from "../components/CrudResource";
import { statusOptions } from "../lifecycle";

const STATUS = statusOptions("portfolio");

export function PortfoliosPage() {
  return (
    <CrudResource
      title="Portfolios"
      path="/admin/portfolios"
      editable
      lifecycle={{ resource: "portfolio" }}
      columns={[
        { key: "code", label: "Code" },
        { key: "name", label: "Name" },
        { key: "status", label: "Status" },
        { key: "default_account_id", label: "Default account" },
      ]}
      fields={[
        { name: "code", label: "Code", required: true },
        { name: "name", label: "Name", required: true },
        { name: "status", label: "Status", type: "select", required: true, options: STATUS, default: "ACTIVE" },
        {
          name: "default_account_id",
          label: "Default account (routing)",
          type: "select",
          optionsPath: "/admin/accounts",
          optionValue: "id",
          optionLabel: "code",
        },
      ]}
    />
  );
}
