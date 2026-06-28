import { CrudResource } from "../components/CrudResource";

const STATUS = [
  { value: "ACTIVE", label: "ACTIVE" },
  { value: "INACTIVE", label: "INACTIVE" },
];

export function AccountsPage() {
  return (
    <CrudResource
      title="Accounts"
      path="/admin/accounts"
      editable
      columns={[
        { key: "code", label: "Code" },
        { key: "broker_connection_code", label: "Broker connection" },
        { key: "external_account_ref", label: "External ref" },
        { key: "status", label: "Status" },
      ]}
      fields={[
        { name: "code", label: "Code", required: true },
        {
          name: "broker_connection_code",
          label: "Broker connection",
          type: "select",
          required: true,
          optionsPath: "/admin/broker-connections",
          optionValue: "code",
          optionLabel: "code",
        },
        { name: "external_account_ref", label: "External account ref", required: true },
        { name: "status", label: "Status", type: "select", required: true, options: STATUS },
      ]}
    />
  );
}
