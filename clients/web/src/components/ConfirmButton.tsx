import { useState, type ComponentProps, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import {
  AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription,
  AlertDialogFooter, AlertDialogHeader, AlertDialogTitle,
} from "@/components/ui/alert-dialog";

/** A button that asks before acting (replaces window.confirm). */
export function ConfirmButton({
  title, description, confirmLabel = "Confirm", onConfirm, children, ...button
}: Omit<ComponentProps<typeof Button>, "onClick" | "title"> & {
  title: ReactNode;
  description?: ReactNode;
  confirmLabel?: string;
  onConfirm: () => unknown;
}) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <Button type="button" {...button} onClick={() => setOpen(true)}>{children}</Button>
      <AlertDialog open={open} onOpenChange={setOpen}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{title}</AlertDialogTitle>
            {description && <AlertDialogDescription>{description}</AlertDialogDescription>}
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Cancel</AlertDialogCancel>
            <AlertDialogAction variant="destructive" onClick={() => void onConfirm()}>{confirmLabel}</AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}
