// Authenticated in-app rotation for the local-admin break-glass credential
// (design doc §5.2). Available while signed in by EITHER method — there is
// exactly one operator in this deployment shape, so it's not gated on the
// session having come from `local:admin` specifically.
//
// The parent (AppShell) owns the error banner — rail width can't show
// inline text — so failures report up via `onError` rather than render here.
import { useMutation } from "@tanstack/react-query";
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Button,
  CopyButton,
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  Spinner,
} from "@e412/rnui-react";
import { KeyRound } from "lucide-react";
import { rotateLocalAdmin } from "../api";

export default function RotateLocalAdmin({
  onError,
}: {
  onError: (message: string | null) => void;
}) {
  const mutation = useMutation({
    mutationFn: rotateLocalAdmin,
    onError: (err) => onError(err instanceof Error ? err.message : "Rotation failed."),
  });

  function handleClick() {
    onError(null);
    mutation.mutate();
  }

  // Closing the dialog resets the mutation, which drops the plaintext
  // password out of memory -- there is no second place it is held, so this
  // is the one and only "forget it" point (never displayed or logged again).
  function handleOpenChange(open: boolean) {
    if (!open) mutation.reset();
  }

  return (
    <>
      <Button
        variant="outline"
        size="sm"
        aria-label="Rotate local admin password"
        title="Rotate local admin password"
        className="size-8 justify-center p-0"
        onClick={handleClick}
        disabled={mutation.isPending}
      >
        {mutation.isPending ? <Spinner className="size-4" /> : <KeyRound className="size-4" />}
      </Button>

      <Dialog open={mutation.isSuccess} onOpenChange={handleOpenChange}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>New local admin password</DialogTitle>
            <DialogDescription>
              Shown once, right now. Argus does not display or store it anywhere else.
            </DialogDescription>
          </DialogHeader>

          <div className="flex items-center gap-2 rounded-lg border border-border bg-muted/40 px-3 py-2">
            <code className="min-w-0 flex-1 select-all break-all font-mono text-sm">
              {mutation.data ?? ""}
            </code>
            <CopyButton value={mutation.data ?? ""} />
          </div>

          <Alert variant="warning">
            <AlertTitle>This will not be shown again</AlertTitle>
            <AlertDescription>
              Copy it into a password manager now -- closing this dialog discards it for good.
            </AlertDescription>
          </Alert>
        </DialogContent>
      </Dialog>
    </>
  );
}
