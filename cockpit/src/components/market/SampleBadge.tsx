import { Badge } from "@/components/ui/badge";

/** Marks a surface fed by made-up data so nobody mistakes it for the market. */
export function SampleBadge() {
  return <Badge variant="outline" className="border-warn text-warn">Sample data</Badge>;
}
