import { useEffect, useState } from "react";

/** The roll CLI's install line, from this web app (it serves `/install.sh`). */
export function useInstallLine(): string {
  const [origin, setOrigin] = useState("https://roll.sh");
  useEffect(() => setOrigin(window.location.origin), []);
  return `curl -fsSL ${origin}/install.sh | sh`;
}
