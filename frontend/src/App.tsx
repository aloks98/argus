// Placeholder shell. Real screens (fleet dashboard, machine detail, terminal) are
// built with their slices using @e412/rnui-react components (PRD §8, §10). The
// dependency is installed; components are introduced during implementation.
export default function App() {
  return (
    <main className="mx-auto max-w-2xl p-8 font-sans">
      <h1 className="text-3xl font-semibold">Argus</h1>
      <p className="mt-2 text-gray-600">
        The hundred-eyed watchman — control-plane skeleton.
      </p>
      <p className="mt-4 text-sm text-gray-500">
        The frontend embed pipeline is wired; screens land with their build slices.
      </p>
    </main>
  );
}
