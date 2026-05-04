import { Link, useRouterState } from "@tanstack/react-router";
import { Database, Files, HardDrive, Upload } from "lucide-react";
import { cn } from "@/lib/utils";

const navItems = [
  { to: "/", label: "Dashboard", icon: Database },
  { to: "/files", label: "Files", icon: Files },
  { to: "/xorbs", label: "Xorbs", icon: HardDrive },
  { to: "/upload", label: "Upload", icon: Upload },
] as const;

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
                to === "/" ? currentPath === "/" : currentPath.startsWith(to);
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
        </div>
      </header>

      <main className="mx-auto max-w-7xl px-6 py-8">{children}</main>
    </div>
  );
}
