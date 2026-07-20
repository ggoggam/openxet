import { Link, useRouterState } from "@tanstack/react-router";
import { Boxes, Cloud, Database, Files, HardDrive, KeyRound, Upload } from "lucide-react";
import { Input } from "@/components/ui/input";
import { setToken, useToken } from "@/lib/auth";
import { cn } from "@/lib/utils";

const navItems = [
  { to: "/", label: "Files", icon: Files },
  { to: "/xorbs", label: "Xorbs", icon: Boxes },
  { to: "/storage", label: "Storage", icon: HardDrive },
  { to: "/s3", label: "S3 Gateway", icon: Cloud },
  { to: "/upload", label: "Upload", icon: Upload },
] as const;

/** The server authenticates /v1 calls with OIDC bearer tokens; paste one here
 * and the UI sends it verbatim. Optional against a dev server with auth off. */
function TokenField() {
  const token = useToken();

  return (
    <div
      className="ml-auto flex items-center gap-2"
      title="Bearer token from your OIDC provider — sent as Authorization on /v1 calls. Optional against a dev server with auth disabled."
    >
      <KeyRound className="size-4 text-muted-foreground" />
      <Input
        type="password"
        placeholder="Bearer token (optional)"
        value={token}
        onChange={(e) => setToken(e.target.value)}
        className="h-8 w-48 text-sm"
      />
    </div>
  );
}

export function Layout({ children }: { children: React.ReactNode }) {
  const router = useRouterState();
  const currentPath = router.location.pathname;

  return (
    <div className="min-h-screen bg-background">
      <header className="sticky top-0 z-50 border-b bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/60">
        <div className="mx-auto flex h-14 max-w-7xl items-center px-6">
          <Link to="/" className="mr-8 flex items-center gap-2 font-semibold">
            <Database className="size-5" />
            <span>OpenXet</span>
          </Link>

          <nav className="flex items-center gap-1">
            {navItems.map(({ to, label, icon: Icon }) => {
              const isActive =
                to === "/"
                  ? currentPath === "/" || currentPath.startsWith("/files")
                  : currentPath.startsWith(to);
              return (
                <Link
                  key={to}
                  to={to}
                  className={cn(
                    "flex items-center gap-1.5 rounded-md px-3 py-1.5 text-sm font-medium transition-colors",
                    isActive
                      ? "bg-accent text-accent-foreground"
                      : "text-muted-foreground hover:bg-accent/50 hover:text-accent-foreground"
                  )}
                >
                  <Icon className="size-4" />
                  {label}
                </Link>
              );
            })}
          </nav>

          <TokenField />
        </div>
      </header>

      <main className="mx-auto max-w-7xl px-6 py-8">{children}</main>
    </div>
  );
}
