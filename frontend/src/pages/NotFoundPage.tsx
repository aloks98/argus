import { Link } from "react-router-dom";
import { EmptyState } from "@e412/rnui-react";

export default function NotFoundPage() {
  return (
    <EmptyState
      title="Page not found"
      description="That URL doesn't match anything in Argus."
      action={<Link to="/machines" className="text-xs underline">Back to machines</Link>}
    />
  );
}
