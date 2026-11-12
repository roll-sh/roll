import { Link as RouterLink, type LinkComponentProps } from "@tanstack/react-router";
import { parseSearch } from "@/lib/search";

type Props = Omit<LinkComponentProps<"a">, "to" | "href" | "search" | "hash" | "params" | "from"> & { href: string };

/**
 * A router link to a URL the page has built, which is how roll's links are written
 * (`changesetHref(…)`, `/dashboard?demo#agents`): a path with its query and hash, handed to the
 * router's `href` option, or a query on the current page (`?state=landed`; `?` for none).
 * Navigation, preloading and the active state are TanStack's.
 */
export function Link({ href, ...props }: Props) {
  const target = href.startsWith("?") ? { to: ".", search: parseSearch(href) } : { href };
  return <RouterLink {...(props as LinkComponentProps<"a">)} {...(target as LinkComponentProps<"a">)} />;
}
