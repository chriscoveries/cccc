// Reading and drawing helpers for the 72-hour harness RAM table.
//
// Kept out of the component file so fast refresh keeps working there.

export const SPARK_WIDTH = 72;
export const SPARK_HEIGHT = 16;

/// MiB as an operator reads it: whole MiB up to a gigabyte, then one decimal.
export function formatMemoryMib(mib: number | null | undefined): string {
  if (mib === null || mib === undefined) return "—";
  if (mib >= 1024) return `${(mib / 1024).toFixed(1)}G`;
  return `${mib}M`;
}

/// Points for the hourly trend line, one per observed hour, in a fixed box.
export function sparklinePoints(
  hours: { avg_rss_mib: number | null }[],
  width = SPARK_WIDTH,
  height = SPARK_HEIGHT,
): string {
  const values = hours.map((hour) => hour.avg_rss_mib ?? 0);
  if (values.length === 0) return "";
  const peak = Math.max(...values, 1);
  const step = values.length > 1 ? width / (values.length - 1) : 0;
  return values
    .map((value, index) => {
      const x = (index * step).toFixed(1);
      const y = (height - (value / peak) * height).toFixed(1);
      return `${x},${y}`;
    })
    .join(" ");
}
