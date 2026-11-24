import { useSearchParams } from "react-router-dom";
import { Frame } from "../../login/Frame";
import { Notice } from "../../components/common";
import { accessMessage } from "../../login";

/** Where a refused sign-in lands. Needs no session: it must render signed out. */
export function AccessPage() {
  const [params] = useSearchParams();
  return (
    <Frame title="Can't sign you in">
      <Notice tone="info">{accessMessage(params.get("reason"), params.get("hint"))}</Notice>
    </Frame>
  );
}
