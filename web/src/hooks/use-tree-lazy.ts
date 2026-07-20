"use client";

import { useState, useCallback, useRef } from "react";
import type {
  TreeNodeData,
  TreeNodeNested,
  FlatTreeNode,
  LoadChildrenFn,
  MaybePromise,
} from "@/lib/tree-types";

export interface UseTreeLazyOptions<T extends TreeNodeData> {
  loadChildren?: LoadChildrenFn<T>;
  insertChildren: (parentId: string, children: TreeNodeNested<T>[]) => void;
  expand: (id: string) => void;
  onLoadError?: (nodeId: string, error: Error) => MaybePromise<void>;
}

export interface UseTreeLazyReturn {
  loadingIds: Set<string>;
  isLoading: (id: string) => boolean;
  triggerLoad: (node: FlatTreeNode) => void;
}

export function useTreeLazy<T extends TreeNodeData>(
  options: UseTreeLazyOptions<T>
): UseTreeLazyReturn {
  const { loadChildren, insertChildren, expand, onLoadError } = options;
  const [loadingIds, setLoadingIds] = useState<Set<string>>(new Set());
  const inFlightRef = useRef<Set<string>>(new Set());

  const isLoading = useCallback(
    (id: string) => loadingIds.has(id),
    [loadingIds]
  );

  const triggerLoad = useCallback(
    (node: FlatTreeNode) => {
      if (!loadChildren) return;
      if (!node.isGroup) return;
      if (node.childrenLoaded) return;
      if (inFlightRef.current.has(node.id)) return;

      inFlightRef.current.add(node.id);
      setLoadingIds((prev) => {
        const next = new Set(prev);
        next.add(node.id);
        return next;
      });

      loadChildren(node as FlatTreeNode<T>)
        .then((children) => {
          insertChildren(node.id, children);
          expand(node.id);
        })
        .catch((error) => {
          if (onLoadError) {
            onLoadError(
              node.id,
              error instanceof Error ? error : new Error(String(error))
            );
          }
        })
        .finally(() => {
          inFlightRef.current.delete(node.id);
          setLoadingIds((prev) => {
            const next = new Set(prev);
            next.delete(node.id);
            return next;
          });
        });
    },
    [loadChildren, insertChildren, expand, onLoadError]
  );

  return { loadingIds, isLoading, triggerLoad };
}
