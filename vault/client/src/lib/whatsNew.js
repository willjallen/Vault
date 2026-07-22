const NOTE_VISUALS = new Map([
  ["build", { icon: "gear", tone: "technical" }],
  ["chore", { icon: "gear", tone: "technical" }],
  ["ci", { icon: "gear", tone: "technical" }],
  ["docs", { icon: "book-open", tone: "technical" }],
  ["feat", { icon: "wand-magic-sparkles", tone: "feature" }],
  ["fix", { icon: "check", tone: "improvement" }],
  ["note", { icon: "star", tone: "feature" }],
  ["perf", { icon: "bolt", tone: "performance" }],
  ["refactor", { icon: "code", tone: "technical" }],
  ["test", { icon: "flask-vial", tone: "technical" }],
]);

function normalizeReleaseNotes(releaseNotes) {
  if (!Array.isArray(releaseNotes)) {
    return [];
  }
  return releaseNotes.flatMap((section) => {
    if (!section || typeof section.version !== "string" || !Array.isArray(section.entries)) {
      return [];
    }
    const entries = section.entries.flatMap((entry) => {
      if (!entry || typeof entry.text !== "string" || !entry.text.trim()) {
        return [];
      }
      return [
        {
          kind: typeof entry.kind === "string" ? entry.kind : "note",
          text: entry.text.trim(),
        },
      ];
    });
    return entries.length ? [{ entries, version: section.version }] : [];
  });
}

export function releaseNotesSince(releaseNotes, currentVersion, acknowledgedVersion) {
  if (typeof currentVersion !== "string" || currentVersion === acknowledgedVersion) {
    return [];
  }
  const sections = normalizeReleaseNotes(releaseNotes);
  const currentSection = sections.find((section) => section.version === currentVersion);
  if (!currentSection) {
    return [];
  }
  if (!acknowledgedVersion) {
    return [currentSection];
  }
  const unseen = [];
  let collecting = false;
  let foundAcknowledgement = false;
  for (const section of sections) {
    collecting ||= section.version === currentVersion;
    if (collecting && section.version === acknowledgedVersion) {
      foundAcknowledgement = true;
      break;
    }
    if (collecting) {
      unseen.push(section);
    }
  }
  return foundAcknowledgement ? unseen : [currentSection];
}

export function releaseNoteVisual(kind) {
  return NOTE_VISUALS.get(kind) || { icon: "star", tone: "technical" };
}
