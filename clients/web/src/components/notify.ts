import { toast } from "sonner";
import { ApiError } from "../api/client";

/** A toast. `color` picks the kind; `autoClose: false` keeps it until dismissed. */
export function notify({ color, title, message, autoClose }: {
  color?: string;
  title?: string;
  message: string;
  autoClose?: boolean | number;
}) {
  const opts = {
    description: title ? message : undefined,
    duration: autoClose === false ? Infinity : typeof autoClose === "number" ? autoClose : undefined,
    closeButton: autoClose === false,
  };
  const text = title ?? message;
  if (color === "red") toast.error(text, opts);
  else if (color === "green" || color === "teal") toast.success(text, opts);
  else if (color === "amber" || color === "yellow" || color === "orange") toast.warning(text, opts);
  else toast(text, opts);
}

/** The red toast for a failed request. */
export function notifyError(err: unknown) {
  const message = err instanceof ApiError ? `${err.status}: ${err.message}` : String(err);
  notify({ message, color: "red" });
}
