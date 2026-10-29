import { useEffect, useState } from 'react';
import { api, type Subject } from './api';

export function useSubjects(): Subject[] {
  const [subjects, setSubjects] = useState<Subject[]>([]);

  useEffect(() => {
    api.listSubjects().then(setSubjects);
  }, []);

  return subjects;
}
