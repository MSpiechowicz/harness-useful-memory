import { Header } from './components/Header';
import { SubjectList } from './components/SubjectList';

export function App() {
  return (
    <main>
      <Header title="Subjects" />
      <SubjectList />
    </main>
  );
}
