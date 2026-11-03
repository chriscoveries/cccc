import { createContext, type ReactNode, useContext, useEffect, useState } from "react";
import { fetchActors } from "../../services/api/actors";
import type { Actor } from "../../types";

type Memory = { total_bytes: number; available_bytes: number };
type Resources = { groupId?: string; actors: Record<string, Actor["resources"]>; memory?: Memory };
const ResourceContext = createContext<Resources>({ actors: {} });

export function ResourceProvider({ groupId, children }: { groupId: string; children: ReactNode }) {
  const [value, setValue] = useState<Resources>({ actors: {} });
  useEffect(() => {
    setValue({ actors: {} });
    if (!groupId) return;
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    const refresh = async () => {
      try {
        const response = await fetchActors(
          groupId,
          false,
          { signal: controller.signal },
          { includeResources: true, includeInternal: true },
        );
        if (!controller.signal.aborted) {
          if (response.ok) {
            const actors = Object.fromEntries(
              response.result.actors.map((actor) => [actor.id, actor.resources]),
            );
            setValue({ groupId, actors, memory: response.result.host_memory });
          } else {
            setValue({ actors: {} });
          }
        }
      } catch {
        if (!controller.signal.aborted) setValue({ actors: {} });
      } finally {
        if (!controller.signal.aborted) timer = setTimeout(refresh, 10_000);
      }
    };
    void refresh();
    return () => {
      controller.abort();
      if (timer) clearTimeout(timer);
    };
  }, [groupId]);
  return (
    <ResourceContext.Provider value={value.groupId === groupId ? value : { actors: {} }}>
      {children}
    </ResourceContext.Provider>
  );
}

function formatBytes(bytes: number): string {
  const mib = bytes / 1048576;
  return mib >= 1024 ? `${(mib / 1024).toFixed(1)} GiB` : `${Math.round(mib)} MiB`;
}

export function HostResourcesBadge() {
  const { memory } = useContext(ResourceContext);
  if (
    !memory ||
    !Number.isFinite(memory.total_bytes) ||
    !Number.isFinite(memory.available_bytes) ||
    memory.available_bytes > memory.total_bytes
  )
    return null;
  const used = memory.total_bytes - memory.available_bytes;
  return (
    <span
      className="shrink-0 whitespace-nowrap text-[10px] tabular-nums text-[var(--color-text-tertiary)]"
      title="Host RAM used / available"
      role="img"
      aria-label={`Host RAM: ${formatBytes(used)} used, ${formatBytes(memory.available_bytes)} available`}
    >
      RAM {formatBytes(used)} / {formatBytes(memory.available_bytes)}
    </span>
  );
}

export function ActorResourcesBadge({
  actorId,
  running = false,
}: {
  actorId: string;
  running?: boolean;
}) {
  const resource = useContext(ResourceContext).actors[actorId];
  if (!resource || !Number.isFinite(resource.rss_bytes)) {
    return running ? (
      <span
        className="shrink-0 whitespace-nowrap text-[10px] tabular-nums text-[var(--color-text-tertiary)]"
        title="Process resources unavailable"
        role="img"
        aria-label="Actor resource usage unavailable"
      >
        — · —
      </span>
    ) : null;
  }
  const cpu =
    resource.cpu_percent == null || !Number.isFinite(resource.cpu_percent)
      ? "—"
      : `${resource.cpu_percent.toFixed(1)}%`;
  return (
    <span
      className="shrink-0 whitespace-nowrap text-[10px] tabular-nums text-[var(--color-text-tertiary)]"
      title="Observed process-tree RSS (shared pages may be counted more than once) · CPU as a percentage of one core"
      role="img"
      aria-label={`Actor memory ${formatBytes(resource.rss_bytes)}, CPU ${cpu}`}
    >
      {formatBytes(resource.rss_bytes)} · {cpu}
    </span>
  );
}
