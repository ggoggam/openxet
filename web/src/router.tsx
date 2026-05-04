import {
  createRootRoute,
  createRoute,
  createRouter,
  Outlet,
} from "@tanstack/react-router";
import { Layout } from "@/components/layout";
import { DashboardPage } from "@/pages/dashboard";
import { FilesPage } from "@/pages/files";
import { FileDetailPage } from "@/pages/file-detail";
import { XorbsPage } from "@/pages/xorbs";
import { UploadPage } from "@/pages/upload";

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
  component: DashboardPage,
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

const xorbsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/xorbs",
  component: XorbsPage,
});

const uploadRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/upload",
  component: UploadPage,
});

const routeTree = rootRoute.addChildren([
  indexRoute,
  filesRoute,
  fileDetailRoute,
  xorbsRoute,
  uploadRoute,
]);

export const router = createRouter({ routeTree });

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
