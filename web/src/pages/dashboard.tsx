import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { Files, HardDrive, Database, Upload } from "lucide-react";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { fetchStats } from "@/lib/api";
import { formatBytes } from "@/lib/format";

export function DashboardPage() {
  const { data: stats, isLoading, error } = useQuery({
    queryKey: ["stats"],
    queryFn: fetchStats,
  });

  if (error) {
    return (
      <div className="text-destructive">
        Failed to load stats: {error.message}
      </div>
    );
  }

  return (
    <div className="space-y-8">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">Dashboard</h1>
        <p className="text-muted-foreground">
          Overview of your Content Addressable Storage server.
        </p>
      </div>

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <StatCard
          title="Files"
          icon={Files}
          value={stats?.files_count}
          loading={isLoading}
          href="/files"
        />
        <StatCard
          title="Xorbs"
          icon={HardDrive}
          value={stats?.xorbs_count}
          loading={isLoading}
          href="/xorbs"
        />
        <StatCard
          title="Shards"
          icon={Database}
          value={stats?.shards_count}
          loading={isLoading}
        />
        <StatCard
          title="Total Storage"
          icon={Upload}
          value={stats ? formatBytes(stats.total_size_bytes) : undefined}
          loading={isLoading}
        />
      </div>
    </div>
  );
}

function StatCard({
  title,
  icon: Icon,
  value,
  loading,
  href,
}: {
  title: string;
  icon: React.ComponentType<{ className?: string }>;
  value?: string | number;
  loading: boolean;
  href?: string;
}) {
  const card = (
    <Card className={href ? "transition-colors hover:bg-accent/50" : undefined}>
      <CardHeader className="flex flex-row items-center justify-between pb-2">
        <CardDescription>{title}</CardDescription>
        <Icon className="size-4 text-muted-foreground" />
      </CardHeader>
      <CardContent>
        {loading ? (
          <Skeleton className="h-7 w-20" />
        ) : (
          <CardTitle className="text-2xl">{value ?? 0}</CardTitle>
        )}
      </CardContent>
    </Card>
  );

  if (href) {
    return <Link to={href}>{card}</Link>;
  }

  return card;
}
