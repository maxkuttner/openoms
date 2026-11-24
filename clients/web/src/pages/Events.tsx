import { useState } from "react";
import { Input } from "@/components/ui/input";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Section } from "../components/common";
import { SampleBadge } from "../components/market/SampleBadge";
import { Pill } from "../components/status";
import { sampleEvents, type BusEvent, type EventsSource } from "../sampleFeeds";

/** Events on the message bus and those published outwards (sample until both exist). */
export function EventsPage({ source = sampleEvents }: { source?: EventsSource }) {
  const [stream, setStream] = useState<BusEvent["stream"]>("bus");
  const [query, setQuery] = useState("");
  const rows = source.events(stream, query);
  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2">
        <Tabs value={stream} onValueChange={(v) => setStream(v as BusEvent["stream"])}>
          <TabsList><TabsTrigger value="bus">Message bus</TabsTrigger><TabsTrigger value="published">Published (Kafka)</TabsTrigger></TabsList>
        </Tabs>
        <Input aria-label="Filter events" placeholder="Topic, key…" value={query} onChange={(e) => setQuery(e.currentTarget.value)} className="h-8 w-56" />
        {source.sample && <SampleBadge />}
      </div>
      <Section className="p-0">
        <Table>
          <TableHeader><TableRow><TableHead>Time</TableHead><TableHead>Topic</TableHead><TableHead>Key</TableHead><TableHead>Summary</TableHead></TableRow></TableHeader>
          <TableBody>
            {rows.map((e) => (
              <TableRow key={`${e.at}${e.topic}`}>
                <TableCell className="num whitespace-nowrap text-muted-foreground">{e.at.slice(11, 23)}</TableCell>
                <TableCell><Pill tone="warn" className="font-mono">{e.topic}</Pill></TableCell>
                <TableCell className="font-mono text-xs">{e.key}</TableCell>
                <TableCell>{e.summary}</TableCell>
              </TableRow>
            ))}
            {rows.length === 0 && <TableRow><TableCell colSpan={4} className="text-center text-muted-foreground">No events.</TableCell></TableRow>}
          </TableBody>
        </Table>
      </Section>
    </div>
  );
}
