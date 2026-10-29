import { useSubjects } from '../hooks';

export function SubjectList() {
  const subjects = useSubjects();

  return (
    <ul>
      {subjects.map((subject) => (
        <li key={subject.id}>{subject.id}</li>
      ))}
    </ul>
  );
}
