import { useEffect, useRef } from "react";
import { Anchor, Card, Code, Container, SimpleGrid, Stack, Text, Title } from "@mantine/core";

// Data model reference: how feeds, instruments and broker connections relate.
// Mermaid is heavy, so this page is lazy-loaded (see App.tsx) and mermaid is
// imported dynamically inside the effect.

const OVERVIEW = `flowchart LR
  DBN["Databento<br/>OPRA options"]:::feed
  BNF["Binance<br/>spot book"]:::feed
  BYF["Bybit<br/>spot book"]:::feed

  FS["FeedSymbology<br/>to_feed_symbol()"]:::feed
  FI[("feed_instrument")]:::feed

  INST[("instrument<br/><b>symbol @ venue</b>")]:::core

  BI[("broker_instrument")]:::exec
  IP["InstrumentProvider<br/>list_instruments()"]:::exec

  ALP["Alpaca<br/>equities · options"]:::exec
  BNB["Binance<br/>crypto spot"]:::exec

  DBN --> FS
  BNF --> FS
  BYF --> FS
  FS -- "map-feed" --> FI
  FI -- "n:1" --> INST
  INST -- "1:n" --> BI
  BI -- "sync-broker" --> IP
  IP --> ALP
  IP --> BNB

  classDef core fill:#e7edf3,stroke:#3a4a5a,color:#16202b;
  classDef exec fill:#f7ecd6,stroke:#b4700e,color:#3a2a06;
  classDef feed fill:#dcf0f6,stroke:#0e7490,color:#05323d;`;

const ER = `erDiagram
  VENUE    ||--o{ INSTRUMENT : "lists (MIC)"
  CURRENCY ||--o{ INSTRUMENT : "quoted in"
  INSTRUMENT ||--o| INSTRUMENT_DERIVATIVE : "option legs"
  INSTRUMENT ||--o{ BROKER_INSTRUMENT : "tradable via"
  INSTRUMENT ||--o{ FEED_INSTRUMENT : "priced by"
  BROKER_CONNECTION ||--o{ ACCOUNT : "routing target"
  INSTRUMENT {
    bigint id PK
    text   symbol "UNIQUE(symbol,venue)"
    text   venue FK
    text   currency FK
    text   asset_class
    text   instrument_class
    text   figi
  }
  INSTRUMENT_DERIVATIVE {
    bigint instrument_id PK,FK
    text   underlying_symbol
    text   option_kind
    numeric strike_price
    date   expiry_date
  }
  BROKER_INSTRUMENT {
    bigint instrument_id FK
    text   broker_code "ALPACA|BINANCE"
    text   broker_symbol
    text   native_id
    bool   is_tradeable
    numeric min_quantity
  }
  FEED_INSTRUMENT {
    text   feed_code "DATABENTO|BINANCE|BYBIT"
    text   feed_symbol
    bigint instrument_id FK
    bool   is_active
  }
  VENUE { text code PK "XNAS, OPRA, BINANCE" }
  CURRENCY { text code PK "USD, USDT" }
  BROKER_CONNECTION {
    text code PK
    text broker_code
    text environment "PAPER|LIVE"
  }
  ACCOUNT { text code PK }`;

const SEED = `flowchart LR
  ALP["Alpaca adapter"]:::exec
  BIN["Binance adapter"]:::exec
  INST[("instrument<br/>+ derivative")]:::core
  BI[("broker_instrument")]:::exec
  FI[("feed_instrument")]:::feed
  ALP -- "sync-broker" --> INST
  BIN -- "sync-broker" --> INST
  ALP -- "sync-broker" --> BI
  BIN -- "sync-broker" --> BI
  INST -- "map-feed" --> FI
  classDef core fill:#e7edf3,stroke:#3a4a5a,color:#16202b;
  classDef exec fill:#f7ecd6,stroke:#b4700e,color:#3a2a06;
  classDef feed fill:#dcf0f6,stroke:#0e7490,color:#05323d;`;

const RUNTIME = `flowchart TB
  subgraph PRICE["Pricing path (market data)"]
    direction LR
    LF["Live feeds<br/>Databento · Binance · Bybit"]:::feed
    QF["subscribe held<br/>via feed_instrument"]:::feed
    MR["mark_router<br/>ranked by provider_feed_policy"]:::feed
    MS[("MarkStore")]:::feed
    PL["positions · M2M P/L"]:::core
    LF --> QF --> MR --> MS --> PL
  end
  subgraph EXEC["Execution path (orders)"]
    direction LR
    ORD["Order"]:::core
    ACC["account -> broker_connection<br/>(broker_code, env)"]:::exec
    BIx["broker_instrument<br/>broker_symbol / native_id"]:::exec
    REG["BrokerRegistry adapter"]:::exec
    API["Broker API"]:::exec
    ORD --> ACC --> REG
    ORD -. "resolve handle" .-> BIx --> REG --> API
  end
  classDef core fill:#e7edf3,stroke:#3a4a5a,color:#16202b;
  classDef exec fill:#f7ecd6,stroke:#b4700e,color:#3a2a06;
  classDef feed fill:#dcf0f6,stroke:#0e7490,color:#05323d;`;

function Diagram({ chart }: { chart: string }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    let cancelled = false;
    (async () => {
      const mermaid = (await import("mermaid")).default;
      mermaid.initialize({
        startOnLoad: false,
        // Don't inject an error graphic into document.body on a parse failure —
        // we render any error inline in this component's own container instead.
        suppressErrorRendering: true,
        theme: "base",
        themeVariables: {
          fontFamily: 'ui-monospace, "SF Mono", Menlo, monospace',
          fontSize: "13px",
          primaryColor: "#eef2f6",
          primaryBorderColor: "#3a4a5a",
          primaryTextColor: "#16202b",
          lineColor: "#5a6b7a",
        },
      });
      if (cancelled || !ref.current) return;
      try {
        const { svg } = await mermaid.render(`m-${Math.random().toString(36).slice(2)}`, chart);
        if (!cancelled && ref.current) ref.current.innerHTML = svg;
      } catch (err) {
        if (!cancelled && ref.current) {
          ref.current.textContent = `Diagram failed to render: ${
            err instanceof Error ? err.message : String(err)
          }`;
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [chart]);
  return (
    <div
      ref={ref}
      style={{
        background: "#f7f9fb",
        border: "1px solid #dde4ea",
        borderRadius: 10,
        padding: 18,
        overflowX: "auto",
      }}
    />
  );
}

const CARDS = [
  {
    tag: "Master",
    color: "#3a4a5a",
    title: "instrument",
    body: (
      <>
        Canonical identity, keyed <Code>(symbol, venue)</Code>. Asset/instrument class, microstructure,
        FIGI. Option legs in <Code>instrument_derivative</Code>.
      </>
    ),
  },
  {
    tag: "Execution",
    color: "#b4700e",
    title: "broker_instrument",
    body: (
      <>
        Tradable mapping — one row per instrument per broker: <Code>broker_symbol</Code>,{" "}
        <Code>native_id</Code>, limits. Written by <Code>sync-broker</Code> from the adapter's{" "}
        <Code>InstrumentProvider</Code>. Its existence means tradable.
      </>
    ),
  },
  {
    tag: "Market data",
    color: "#0e7490",
    title: "feed_instrument",
    body: (
      <>
        Pricing mapping, 1:n. A feed's <Code>feed_symbol</Code> → instrument(s); one symbol can price
        the pair on several venues. Built by <Code>map-feed</Code>, using each feed's own{" "}
        <Code>FeedSymbology</Code>.
      </>
    ),
  },
];

export function ArchitecturePage() {
  return (
    <Container size="lg" px={0}>
      <Stack gap="xl">
        <div>
          <Title order={2}>Instrument model</Title>
          <Text c="dimmed" mt={6} maw={680}>
            One canonical <b>instrument</b> catalog in the middle, two independent mappings off it: a
            broker's tradable handle (<Code>broker_instrument</Code>) and a data feed's pricing symbol (
            <Code>feed_instrument</Code>). Priceable and tradable are independent — each is just the
            existence of a row.
          </Text>
        </div>

        <SimpleGrid cols={{ base: 1, sm: 3 }} spacing="md">
          {CARDS.map((c) => (
            <Card key={c.title} withBorder padding="md" radius="md">
              <Text
                fz={11}
                fw={700}
                tt="uppercase"
                style={{ letterSpacing: "0.06em", color: c.color, fontFamily: "ui-monospace, monospace" }}
              >
                {c.tag}
              </Text>
              <Text fw={600} mt={4} mb={6} style={{ fontFamily: "ui-monospace, monospace" }}>
                {c.title}
              </Text>
              <Text fz="sm" c="dimmed">
                {c.body}
              </Text>
            </Card>
          ))}
        </SimpleGrid>

        <div>
          <Title order={4} mb={4}>
            End to end
          </Title>
          <Text c="dimmed" fz="sm" mb="sm" maw={680}>
            One catalog in the middle, an adapter at each end. A feed translates its own symbols
            through <Code>FeedSymbology</Code>; a broker publishes its tradable catalog through{" "}
            <Code>InstrumentProvider</Code>. Each adapter owns its own symbology, so neither end
            knows the other exists.
          </Text>
          <Diagram chart={OVERVIEW} />
        </div>

        <div>
          <Title order={4} mb={4}>
            Tables &amp; relationships
          </Title>
          <Text c="dimmed" fz="sm" mb="sm" maw={680}>
            Foreign keys shown. <Code>broker_instrument</Code> and <Code>feed_instrument</Code> each
            reference the master instrument; neither references the other. Two soft links by code (no
            FK): <Code>broker_code</Code> → <Code>broker_connection</Code>, and <Code>feed_code</Code>{" "}
            is ranked for failover in <Code>provider_feed_policy</Code>.
          </Text>
          <Diagram chart={ER} />
        </div>

        <div>
          <Title order={4} mb={4}>
            Seeding — where rows come from
          </Title>
          <Text c="dimmed" fz="sm" mb="sm" maw={680}>
            Broker-first: <Code>sync-broker</Code> creates the master catalog + broker mapping;{" "}
            <Code>map-feed</Code> then maps feeds onto it. <Code>make seed-live</Code> runs the whole
            chain idempotently.
          </Text>
          <Diagram chart={SEED} />
        </div>

        <div>
          <Title order={4} mb={4}>
            Runtime — the two paths
          </Title>
          <Text c="dimmed" fz="sm" mb="sm" maw={680}>
            Pricing reads <Code>feed_instrument</Code>; order routing reads <Code>broker_instrument</Code>.
            They never cross — a feed can price an instrument no broker trades, and vice versa.
          </Text>
          <Diagram chart={RUNTIME} />
        </div>

        <Text fz="xs" c="dimmed">
          Reflects migrations 0016–0019 and the <Code>sync-broker</Code> / <Code>map-feed</Code> flow.
          See also{" "}
          <Anchor href="/api-docs" fz="xs">
            API docs
          </Anchor>
          .
        </Text>
      </Stack>
    </Container>
  );
}
