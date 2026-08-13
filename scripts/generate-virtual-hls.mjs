#!/usr/bin/env node

import { readFile, writeFile } from "node:fs/promises";

function usage(message) {
  if (message) {
    console.error(message);
  }
  console.error(
    "usage: generate-virtual-hls.mjs --base-url URL --output FILE " +
      "[--parts-per-segment N] INDEX.json [INDEX.json ...]",
  );
  process.exit(2);
}

const args = process.argv.slice(2);
let baseUrl;
let output;
let partsPerSegment = 1;
const indexPaths = [];

for (let i = 0; i < args.length; i += 1) {
  switch (args[i]) {
    case "--base-url":
      baseUrl = args[++i];
      break;
    case "--output":
      output = args[++i];
      break;
    case "--parts-per-segment":
      partsPerSegment = Number(args[++i]);
      break;
    case "--help":
    case "-h":
      usage();
      break;
    default:
      if (args[i].startsWith("-")) {
        usage(`unknown option: ${args[i]}`);
      }
      indexPaths.push(args[i]);
  }
}

if (!baseUrl || !output || indexPaths.length === 0) {
  usage("--base-url, --output, and at least one index are required");
}
if (!Number.isSafeInteger(partsPerSegment) || partsPerSegment < 1) {
  usage("--parts-per-segment must be a positive integer");
}

baseUrl = baseUrl.replace(/\/+$/, "");

function objectUrl(key) {
  return `${baseUrl}/${key.split("/").map(encodeURIComponent).join("/")}`;
}

function decimalSeconds(nanoseconds) {
  const value = BigInt(nanoseconds);
  const seconds = value / 1_000_000_000n;
  const fraction = value % 1_000_000_000n;
  if (fraction === 0n) {
    return `${seconds}.0`;
  }
  return `${seconds}.${fraction.toString().padStart(9, "0").replace(/0+$/, "")}`;
}

function programDateTime(nanoseconds) {
  const value = BigInt(nanoseconds);
  const seconds = value / 1_000_000_000n;
  const fraction = value % 1_000_000_000n;
  const whole = new Date(Number(seconds) * 1_000).toISOString().slice(0, 19);
  if (fraction === 0n) {
    return `${whole}Z`;
  }
  return `${whole}.${fraction.toString().padStart(9, "0").replace(/0+$/, "")}Z`;
}

function groupParts(parts, count) {
  const groups = [];
  for (let start = 0; start < parts.length; start += count) {
    const group = parts.slice(start, start + count);
    const first = group[0];
    let end = BigInt(first.offset) + BigInt(first.length);
    let duration = 0n;

    for (let i = 0; i < group.length; i += 1) {
      const part = group[i];
      if (i > 0 && BigInt(part.offset) !== end) {
        throw new Error(
          `parts ${group[i - 1].sequence_number} and ${part.sequence_number} are not contiguous`,
        );
      }
      end = BigInt(part.offset) + BigInt(part.length);
      duration += BigInt(part.presentation_duration_ns ?? part.duration_ns);
    }

    groups.push({
      offset: BigInt(first.offset),
      length: end - BigInt(first.offset),
      duration,
      startUtcNs: BigInt(first.start_utc_ns),
    });
  }
  return groups;
}

const indexes = await Promise.all(
  indexPaths.map(async (path) => JSON.parse(await readFile(path, "utf8"))),
);

let maximumDuration = 0n;
const recordings = indexes.map((index) => {
  if (!Array.isArray(index.parts) || index.parts.length === 0) {
    throw new Error(`${index.source?.object_key ?? "index"} contains no parts`);
  }
  const groups = groupParts(index.parts, partsPerSegment);
  for (const group of groups) {
    maximumDuration = group.duration > maximumDuration ? group.duration : maximumDuration;
  }
  return { index, groups };
});

const targetDuration = (maximumDuration + 999_999_999n) / 1_000_000_000n;
const lines = [
  "#EXTM3U",
  "#EXT-X-VERSION:7",
  `#EXT-X-TARGETDURATION:${targetDuration > 0n ? targetDuration : 1n}`,
  "#EXT-X-MEDIA-SEQUENCE:0",
  "#EXT-X-PLAYLIST-TYPE:VOD",
];

let activeInitHash;
for (let recordingIndex = 0; recordingIndex < recordings.length; recordingIndex += 1) {
  const { index, groups } = recordings[recordingIndex];
  if (recordingIndex > 0) {
    lines.push("#EXT-X-DISCONTINUITY");
  }
  if (index.init.sha256 !== activeInitHash) {
    lines.push(
      `#EXT-X-MAP:URI="${objectUrl(`init/${index.init.sha256}.mp4`)}"`,
    );
    activeInitHash = index.init.sha256;
  }

  for (const group of groups) {
    lines.push(
      `#EXT-X-PROGRAM-DATE-TIME:${programDateTime(group.startUtcNs)}`,
      `#EXTINF:${decimalSeconds(group.duration)},`,
      `#EXT-X-BYTERANGE:${group.length}@${group.offset}`,
      objectUrl(index.source.object_key),
    );
  }
}

lines.push("#EXT-X-ENDLIST", "");
await writeFile(output, lines.join("\n"));
