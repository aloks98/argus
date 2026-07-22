import { Routes, Route } from "react-router-dom";
import FleetPage from "./FleetPage";
import MachineDetailPage from "./MachineDetailPage";
import ThemeToggle from "./ThemeToggle";

export default function App() {
  return (
    <>
      <div className="fixed right-4 top-4 z-50">
        <ThemeToggle />
      </div>
      <Routes>
        <Route path="/" element={<FleetPage />} />
        <Route path="/machines/:id" element={<MachineDetailPage />} />
      </Routes>
    </>
  );
}
