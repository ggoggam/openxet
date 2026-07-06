import { Link, useRouterState } from "@tanstack/react-router";
import { Database, Files, KeyRound, Upload } from "lucide-react";
import { Input } from "@/components/ui/input";
import { setSecret, useSecret } from "@/lib/auth";
import { cn } from "@/lib/utils";

const navItems = [
  { to: "/", label: "Files", icon: Files },
  { to: "/upload", label: "Upload", icon: Upload },
] as const;

/** The server authenticates /v1 calls with JWTs signed by its
 * OPENXET_AUTH_SECRET; the UI mints tokens locally from this secret. */
function SecretField() {
  const secret = useSecret();

  return (
    <div
      className="ml-auto flex items-center gap-2"
      title="OPENXET_AUTH_SECRET of the server — used to mint access tokens in this browser"
    >
      <KeyRound
        className={cn(
          "size-4",
          secret ? "text-muted-foreground" : "text-amber-500",
        )}
      />
      <Input
        type="password"
        placeholder="Auth secret required"
        value={secret}
        onChange={(e) => setSecret(e.target.value)}
        className={cn(
          "h-8 w-48 text-sm",
          !secret && "border-amber-500 ring-1 ring-amber-500/50",
        )}
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

          <SecretField />
        </div>
      </header>

      <main className="mx-auto max-w-7xl px-6 py-8">{children}</main>
    </div>
  );
}
