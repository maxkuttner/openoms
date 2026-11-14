import { Alert, Container, Text } from "@mantine/core";
import { useSearchParams } from "react-router-dom";
import { accessMessage } from "../../login";

/** Where a refused sign-in lands. Needs no session: it must render signed out. */
export function AccessPage() {
  const [params] = useSearchParams();
  return (
    <Container size="sm" py="xl">
      <Alert color="amber" title="Can't sign you in">
        <Text size="sm">{accessMessage(params.get("reason"), params.get("hint"))}</Text>
      </Alert>
    </Container>
  );
}
