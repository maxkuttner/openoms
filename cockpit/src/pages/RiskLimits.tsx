import { CrudResource } from "../components/CrudResource";

const TRADING_STATE = [
  { value: "ACTIVE", label: "ACTIVE" },
  { value: "REDUCING", label: "REDUCING" },
  { value: "HALTED", label: "HALTED" },
];

export function RiskLimitsPage() {
  return (
    <CrudResource
      title="Risk limits"
      path="/admin/risk-limits"
      editable
      deletable
      columns={[
        { key: "portfolio_id", label: "Portfolio" },
        { key: "instrument_id", label: "Instrument" },
        { key: "trading_state", label: "State" },
        { key: "max_order_quantity", label: "Max order qty" },
        { key: "max_order_notional", label: "Max order $" },
        { key: "max_position_quantity", label: "Max pos qty" },
        { key: "max_position_notional", label: "Max pos $" },
      ]}
      fields={[
        {
          name: "portfolio_id",
          label: "Portfolio",
          type: "select",
          required: true,
          inEdit: false,
          optionsPath: "/admin/portfolios",
          optionValue: "id",
          optionLabel: "code",
        },
        { name: "instrument_id", label: "Instrument id", required: true, inEdit: false },
        { name: "trading_state", label: "Trading state", type: "select", options: TRADING_STATE },
        { name: "max_order_quantity", label: "Max order quantity", type: "number" },
        { name: "max_order_notional", label: "Max order notional", type: "number" },
        { name: "max_position_quantity", label: "Max position quantity", type: "number" },
        { name: "max_position_notional", label: "Max position notional", type: "number" },
      ]}
    />
  );
}
