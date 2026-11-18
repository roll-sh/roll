import { useEffect, useState } from "react";
import { loadInventory, resourcesByProduct } from "@/lib/workspace-resources";
import { useWorkspace } from "@/lib/workspace-store";

/** Names for the ids rules and approvals hold: projects', and each product's resources'. An unknown id stays as it is. */
export function useResourceNames() {
  const ws = useWorkspace();
  const [inventory, setInventory] = useState<Record<string, { id: string; name: string }[]>>({});
  useEffect(() => {
    loadInventory()
      .then((inv) => setInventory(resourcesByProduct(inv)))
      .catch(() => {});
  }, []);
  const projectName = (p: string) => ws.projects.find((x) => x.id === p)?.name ?? p;
  const resourceName = (product: string, r: string) => inventory[product]?.find((x) => x.id === r)?.name ?? (product === "workspace" ? projectName(r) : r);
  return { projectName, resourceName };
}
