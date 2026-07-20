"use client";

import { cn } from "@/lib/utils";

interface TreeDropIndicatorProps {
  depth: number;
  indentationWidth: number;
  className?: string;
}

export function TreeDropIndicator({
  depth,
  indentationWidth,
  className,
}: TreeDropIndicatorProps) {
  return (
    <div
      data-slot="tree-drop-indicator"
      className={cn("pointer-events-none absolute right-0 bottom-0 h-0.5 bg-primary", className)}
      style={{
        left: depth * indentationWidth,
      }}
    />
  );
}
