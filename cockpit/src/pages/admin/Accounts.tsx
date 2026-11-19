import { CrudResource } from "../../components/CrudResource";
import { Notice } from "../../components/common";
import { statusOptions } from "../../lifecycle";
import { useList } from "../../api/hooks";
import { showAccounts } from "../../recon";
import type { Venue } from "../../venues";
import type { BrokerConnection } from "../../api/types";

const STATUS = statusOptions("account");

// Only multi-account venues (IBKR) have accounts to manage: one credential
// reaches several. Every other connection gets its single account created
// automatically, so those are not listed here.
export function AccountsPage() {
  const venues = useList<Venue & { multi_account: boolean }>("/admin/venues").data ?? [];
  const connections = useList<BrokerConnection>("/admin/broker-connections").data ?? [];
  const multi = new Set(connections.filter((c) => showAccounts(c, venues)).map((c) => c.code));
  if (multi.size === 0) {
    return (
      <Notice tone="info" title="No accounts to manage">
        Accounts are created automatically for single-account venues such as Binance. Only a multi-account connection (IBKR) lists them here.
      </Notice>
    );
  }
  return (
    <CrudResource
      rowFilter={(row) => multi.has(row.broker_connection_code)}
      title="Accounts"
      path="/admin/accounts"
      editable
      lifecycle={{ resource: "account" }}
      columns={[
        { key: "code", label: "Code" },
        { key: "broker_connection_code", label: "Broker connection" },
        { key: "external_account_ref", label: "External ref" },
        { key: "portfolio_id", label: "Portfolio" },
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
          optionFilter: (c) => multi.has(c.code),
        },
        { name: "external_account_ref", label: "External account ref", required: true },
        {
          name: "portfolio_id",
          label: "Portfolio",
          type: "select",
          optionsPath: "/admin/portfolios",
          optionValue: "id",
          optionLabel: "code",
        },
        { name: "status", label: "Status", type: "select", required: true, options: STATUS, default: "ACTIVE" },
      ]}
    />
  );
}
