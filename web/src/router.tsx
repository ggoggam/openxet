import {
  createRootRoute,
  createRoute,
  createRouter,
  Outlet,
} from "@tanstack/react-router";
import { Layout } from "@/components/layout";
import { FilesPage } from "@/pages/files";
import { FileDetailPage } from "@/pages/file-detail";
import { UploadPage } from "@/pages/upload";
import { XorbsPage } from "@/pages/xorbs";
import { StoragePage } from "@/pages/storage";

const rootRoute = createRootRoute({
  component: () => (
    <Layout>
      <Outlet />
    </Layout>
  ),
});

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: FilesPage,
});

const filesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/files",
  component: FilesPage,
});

const fileDetailRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/files/$hash",
  component: FileDetailPage,
});

const uploadRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/upload",
  component: UploadPage,
});

const xorbsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/xorbs",
  component: XorbsPage,
});

const storageRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/storage",
  component: StoragePage,
});

const routeTree = rootRoute.addChildren([
  indexRoute,
  filesRoute,
  fileDetailRoute,
  uploadRoute,
  xorbsRoute,
  storageRoute,
]);

export const router = createRouter({ routeTree });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
