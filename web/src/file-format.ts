import type { FileEntry } from "./types";

export function formatBinarySize(raw: string) {
  if (!/^(0|[1-9]\d*)$/.test(raw)) return raw;
  const bytes = Number(raw);
  if (!Number.isSafeInteger(bytes)) return raw;
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let amount = bytes / 1024;
  let unit = units[0];
  for (let index = 1; index < units.length && amount >= 1024; index += 1) {
    amount /= 1024;
    unit = units[index];
  }
  return `${amount.toLocaleString(undefined, { maximumFractionDigits: 1 })} ${unit}`;
}

export function formatModifiedTime(raw: string) {
  const timestamp = Date.parse(raw);
  if (!Number.isFinite(timestamp)) return raw;
  try {
    return new Intl.DateTimeFormat("ko-KR", {
      year: "numeric", month: "short", day: "numeric", hour: "numeric", minute: "2-digit",
    }).format(new Date(timestamp));
  } catch {
    return raw;
  }
}

export function formatFileSummary(files: FileEntry[]) {
  const count = files.length;
  if (count === 0) return "파일 없음";
  let total = 0n;
  for (const file of files) {
    if (!/^(0|[1-9]\d*)$/.test(file.size)) return `${count}개 파일 · 크기 확인 불가`;
    total += BigInt(file.size);
  }
  const size = total <= BigInt(Number.MAX_SAFE_INTEGER) ? formatBinarySize(total.toString()) : `${total} B`;
  return `${count}개 파일 · ${size}`;
}
