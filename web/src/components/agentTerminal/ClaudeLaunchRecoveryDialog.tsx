import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  inspectActorClaudeLaunch,
  resetActorClaudeLaunch,
  type ClaudeLaunchInspection,
} from "../../services/api/actors";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "../ui/dialog";
import { Button } from "../ui/button";

export function ClaudeLaunchRecoveryDialog({
  open,
  onOpenChange,
  groupId,
  actorId,
  onReset,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  groupId: string;
  actorId: string;
  onReset: () => void;
}) {
  const { t } = useTranslation("actors");
  const [inspection, setInspection] = useState<ClaudeLaunchInspection | null>(null);
  const [acknowledged, setAcknowledged] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => {
    if (!open) return;
    let active = true;
    setInspection(null);
    setAcknowledged(false);
    setError("");
    void inspectActorClaudeLaunch(groupId, actorId)
      .then((response) => {
        if (!active) return;
        if (response.ok) setInspection(response.result);
        else setError(response.error?.message || t("claudeLaunchInspectionFailed"));
      })
      .catch((failure: unknown) => {
        if (active) setError(String(failure));
      });
    return () => {
      active = false;
    };
  }, [open, groupId, actorId, t]);
  const reset = async () => {
    if (!inspection?.resettable || !acknowledged || busy) return;
    setBusy(true);
    setError("");
    try {
      const response = await resetActorClaudeLaunch(inspection);
      if (!response.ok) setError(response.error?.message || t("claudeLaunchResetFailed"));
      else {
        onReset();
        onOpenChange(false);
      }
    } catch (failure) {
      setError(String(failure));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="gap-4 p-5">
        <DialogTitle>{t("reconcileClaudeLaunch")}</DialogTitle>
        <DialogDescription>{t("claudeLaunchResetDescription")}</DialogDescription>
        {inspection ? (
          <>
            <div className="break-all font-mono text-xs">{inspection.config_dir}</div>
            {inspection.inspection_error ? <p role="alert">{inspection.inspection_error}</p> : null}
            <pre className="max-h-60 overflow-auto whitespace-pre-wrap break-words text-xs">
              {JSON.stringify(inspection.jobs, null, 2)}
            </pre>
            {inspection.resettable ? (
              <label className="flex items-start gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={acknowledged}
                  onChange={(event) => setAcknowledged(event.target.checked)}
                />
                {t("claudeLaunchResetAcknowledge")}
              </label>
            ) : null}
          </>
        ) : null}
        {error ? <p role="alert">{error}</p> : null}
        <Button
          disabled={!inspection?.resettable || !acknowledged || busy}
          onClick={() => void reset()}
        >
          {t("clearClaudeLaunchFence")}
        </Button>
      </DialogContent>
    </Dialog>
  );
}
